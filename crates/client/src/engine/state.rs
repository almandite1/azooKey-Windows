use std::sync::{LazyLock, Mutex, MutexGuard};

use super::ipc_service::IPCService;

/// State shared across every UI thread of the host process.
///
/// Only genuinely process-wide state lives here — by now that is just the
/// IPC connection (one server session per host process). Everything
/// activation-scoped lives in the per-instance `TextService` instead: TSF
/// activates one TIP per UI thread, and sharing per-activation state here
/// made thread B overwrite thread A's (sink cookies/layout context, B14;
/// the input mode moved out for the same reason).
/// Everything stored here is naturally Send + Sync; do NOT add COM interface
/// pointers (or `unsafe impl Send/Sync` to smuggle them past the compiler).
#[derive(Debug)]
pub struct IMEState {
    pub ipc_service: Option<IPCService>,
}

pub static IME_STATE: LazyLock<Mutex<IMEState>> = LazyLock::new(|| {
    tracing::debug!("Creating IMEState");
    Mutex::new(IMEState { ipc_service: None })
});

impl IMEState {
    pub fn get() -> anyhow::Result<MutexGuard<'static, IMEState>> {
        // TSF runs in a single-threaded apartment, so contention here means
        // re-entrancy (a TSF callback fired while another borrow was alive);
        // failing the call is safer than deadlocking. A poisoned lock (a
        // panic while held) is recovered instead of permanently disabling
        // the IME.
        match IME_STATE.try_lock() {
            Ok(guard) => Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                anyhow::bail!("IMEState is already borrowed (re-entrant TSF callback)")
            }
        }
    }
}
