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

static SESSION_LAST_USED: LazyLock<Mutex<HashMap<i32, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Extracts the pipe-connection session id tonic stored in the request.
pub(crate) fn session_of<T>(request: &Request<T>) -> i32 {
    let id = request
        .extensions()
        .get::<PipeConnectInfo>()
        .map(|info| info.session_id)
        .unwrap_or(0);
    touch_session(id);
    id
}

fn touch_session(id: i32) {
    let mut map = SESSION_LAST_USED
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    map.insert(id, Instant::now());

    let expired: Vec<i32> = map
        .iter()
        .filter(|(sid, last)| **sid != id && last.elapsed() > SESSION_IDLE_TIMEOUT)
        .map(|(sid, _)| *sid)
        .collect();
    for sid in expired {
        map.remove(&sid);
        unsafe { RemoveSession(sid) };
    }
}
