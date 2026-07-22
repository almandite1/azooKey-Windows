//! Session bookkeeping: which pipe connection maps to which engine session,
//! and eviction of engine state for connections that went away.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use azookey_server::PipeConnectInfo;
use tonic::Request;

use crate::ffi::RemoveSession;

/// Sessions are created implicitly on first use, but nothing tells the
/// server when a client connection goes away — evict engine state that has
/// been idle for a while so exited applications don't accumulate sessions.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

static SESSION_LAST_USED: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Extracts the pipe-connection session id tonic stored in the request.
pub(crate) fn session_of<T>(request: &Request<T>) -> i64 {
    let id = request
        .extensions()
        .get::<PipeConnectInfo>()
        .map(|info| info.session_id)
        .unwrap_or(0);
    touch_session(id);
    id
}

/// Pure decision: which sessions other than `current` have been idle past the
/// timeout as of `now`. Separated from `touch_session` so it can be tested
/// without a live engine — the eviction it feeds is an FFI call.
fn expired_sessions(map: &HashMap<i64, Instant>, current: i64, now: Instant) -> Vec<i64> {
    map.iter()
        .filter(|(sid, last)| **sid != current && now.duration_since(**last) > SESSION_IDLE_TIMEOUT)
        .map(|(sid, _)| *sid)
        .collect()
}

fn touch_session(id: i64) {
    // Collect and drop the expired ids from the map, then release the lock
    // BEFORE calling into Swift. RemoveSession is an FFI call; running it
    // while holding SESSION_LAST_USED would, if it ever hung or panicked,
    // stall or poison the lock for every other request that needs it.
    let expired: Vec<i64> = {
        let mut map = SESSION_LAST_USED
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        map.insert(id, Instant::now());

        let expired = expired_sessions(&map, id, Instant::now());
        for sid in &expired {
            map.remove(sid);
        }
        expired
    };

    for sid in expired {
        unsafe { RemoveSession(sid) };
    }
}

#[cfg(test)]
mod tests {
    use super::{expired_sessions, SESSION_IDLE_TIMEOUT};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// A map whose entries were all last used "now", plus the `now` the
    /// assertions advance from. Time moves FORWARD from a real `Instant`:
    /// building a stale timestamp as `Instant::now() - 31min` can underflow
    /// on a CI runner that booted seconds ago.
    fn map_of(ids: &[i64]) -> (HashMap<i64, Instant>, Instant) {
        let start = Instant::now();
        (ids.iter().map(|id| (*id, start)).collect(), start)
    }

    #[test]
    fn a_session_idle_past_the_timeout_is_evicted() {
        let (map, start) = map_of(&[7]);
        let now = start + SESSION_IDLE_TIMEOUT + Duration::from_secs(1);

        assert_eq!(expired_sessions(&map, 1, now), vec![7]);
    }

    /// The comparison is strictly `>`, so a session sitting exactly on the
    /// timeout survives one more round.
    #[test]
    fn a_session_at_exactly_the_timeout_is_kept() {
        let (map, start) = map_of(&[7]);
        let now = start + SESSION_IDLE_TIMEOUT;

        assert!(expired_sessions(&map, 1, now).is_empty());
    }

    #[test]
    fn a_touched_session_is_kept() {
        let (mut map, start) = map_of(&[7]);
        let now = start + SESSION_IDLE_TIMEOUT + Duration::from_secs(1);
        // what touch_session's insert does on every request
        map.insert(7, now);

        assert!(expired_sessions(&map, 1, now).is_empty());
    }

    /// The session making the request is in use by definition, however long
    /// it sat idle before this call.
    #[test]
    fn the_current_session_is_never_evicted() {
        let (map, start) = map_of(&[7]);
        let now = start + SESSION_IDLE_TIMEOUT + Duration::from_secs(1);

        assert!(expired_sessions(&map, 7, now).is_empty());
    }

    /// Eviction is not one-at-a-time: every idle session goes in one pass.
    #[test]
    fn all_expired_sessions_are_collected() {
        let (map, start) = map_of(&[1, 2, 3]);
        let now = start + SESSION_IDLE_TIMEOUT + Duration::from_secs(1);

        let mut expired = expired_sessions(&map, 3, now);
        // HashMap iteration order is arbitrary
        expired.sort_unstable();
        assert_eq!(expired, vec![1, 2], "the current session 3 stays");
    }
}
