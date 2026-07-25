//! Session bookkeeping: which pipe connection maps to which engine session,
//! and eviction of engine state for connections that went away.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use azookey_server::PipeConnectInfo;
use tonic::Request;

use crate::wrappers::remove_session;

/// Sessions are created implicitly on first use, but nothing tells the
/// server when a client connection goes away — evict engine state that has
/// been idle for a while so exited applications don't accumulate sessions.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

static SESSION_LAST_USED: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Extracts the pipe-connection session id tonic stored in the request, and
/// marks that session as used.
pub(crate) fn session_of<T>(request: &Request<T>) -> i64 {
    let id = session_id_of(request);
    touch_session(id);
    id
}

/// The session id tonic recorded on this request, or 0.
///
/// The extension is missing only when the request did not come through our
/// own pipe listener (an in-process caller, a future transport). Falling back
/// to a fixed id rather than refusing keeps such a caller working — it just
/// shares one engine session, which is what every client had before sessions
/// existed. Split out from `session_of` because it is the half that can be
/// tested: `touch_session` calls into Swift.
fn session_id_of<T>(request: &Request<T>) -> i64 {
    request
        .extensions()
        .get::<PipeConnectInfo>()
        .map(|info| info.session_id)
        .unwrap_or(0)
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

/// Marks `id` as used as of `now` and drops the sessions that have gone idle,
/// returning them so the caller can evict them from the engine.
///
/// The map update is the whole of the testable part, and it is deliberately
/// all that happens under the lock: `RemoveSession` is an FFI call, and
/// running it while holding `SESSION_LAST_USED` would, if it ever hung or
/// panicked, stall or poison the lock for every other request that needs it.
/// A lock already poisoned by somebody else is recovered rather than
/// propagated — an idle-session ledger is not worth failing requests over.
fn touch_and_take_expired(id: i64, now: Instant) -> Vec<i64> {
    let mut map = SESSION_LAST_USED
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    map.insert(id, now);

    let expired = expired_sessions(&map, id, now);
    for sid in &expired {
        map.remove(sid);
    }
    expired
}

fn touch_session(id: i64) {
    for sid in touch_and_take_expired(id, Instant::now()) {
        remove_session(sid);
    }
}

#[cfg(test)]
mod tests {
    //! IMPORTANT: nothing here may reference an FFI symbol, directly or
    //! through a helper that calls one — see the note in `wrappers.rs`. That
    //! is why these drive `touch_and_take_expired` and `session_id_of` rather
    //! than `touch_session`/`session_of`, which reach `RemoveSession` through
    //! `wrappers::remove_session`.

    use super::{
        SESSION_IDLE_TIMEOUT, SESSION_LAST_USED, expired_sessions, session_id_of,
        touch_and_take_expired,
    };
    use azookey_server::PipeConnectInfo;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use tonic::Request;

    /// Serializes the tests that drive the process-global `SESSION_LAST_USED`
    /// map: they hand `touch_and_take_expired` a `now` far in the future,
    /// which would otherwise evict a parallel test's session out from under it.
    fn global_map_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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

    /// A request that came through the pipe listener carries its connection's
    /// id; that id is what keys the per-application composing state in the
    /// Swift engine.
    #[test]
    fn the_connections_session_id_is_taken_from_the_request() {
        let mut request = Request::new(());
        request
            .extensions_mut()
            .insert(PipeConnectInfo { session_id: 42 });

        assert_eq!(session_id_of(&request), 42);
    }

    /// Without the extension there is no connection to key on. Falling back
    /// to 0 keeps such a caller working on a single shared engine session —
    /// refusing the request instead would be a hard failure for something
    /// that was never a security boundary (the id is assigned by us, not
    /// sent by the client).
    #[test]
    fn a_request_without_connect_info_falls_back_to_session_zero() {
        assert_eq!(session_id_of(&Request::new(())), 0);
    }

    /// The map half of `touch_session`: the caller's own session is recorded,
    /// and nothing that is still fresh gets swept up with it.
    #[test]
    fn touching_a_session_records_it_and_evicts_nobody_fresh() {
        let _serialize = global_map_lock();
        let now = Instant::now();

        assert!(touch_and_take_expired(0x5100, now).is_empty());
        let neighbour = touch_and_take_expired(0x5101, now);

        assert!(
            !neighbour.contains(&0x5100),
            "a session touched a moment ago is not idle"
        );
        assert!(last_used(0x5100).is_some(), "and it is still on the books");
        assert!(last_used(0x5101).is_some());
    }

    /// The eviction the FFI call consumes: an idle session is returned once,
    /// and is gone from the map — so the next request does not ask the engine
    /// to remove it again.
    #[test]
    fn an_idle_session_is_reported_once_and_removed_from_the_map() {
        let _serialize = global_map_lock();
        let start = Instant::now();
        touch_and_take_expired(0x5200, start);

        let later = start + SESSION_IDLE_TIMEOUT + Duration::from_secs(1);
        let expired = touch_and_take_expired(0x5201, later);

        assert!(expired.contains(&0x5200), "evicted: {expired:?}");
        assert!(last_used(0x5200).is_none(), "and dropped from the map");

        let again = touch_and_take_expired(0x5202, later);
        assert!(!again.contains(&0x5200), "not evicted twice: {again:?}");
    }

    /// A panic under the lock must not take session bookkeeping — hence every
    /// request — down with it.
    #[test]
    fn a_poisoned_map_is_recovered() {
        let _serialize = global_map_lock();

        let poisoner = std::thread::spawn(|| {
            let _held = SESSION_LAST_USED.lock();
            panic!("poisoning the session map on purpose");
        });
        assert!(poisoner.join().is_err(), "the thread must have panicked");
        assert!(SESSION_LAST_USED.is_poisoned());

        touch_and_take_expired(0x5300, Instant::now());
        assert!(last_used(0x5300).is_some());
    }

    fn last_used(id: i64) -> Option<Instant> {
        SESSION_LAST_USED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .copied()
    }
}
