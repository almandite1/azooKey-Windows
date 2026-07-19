use std::sync::{LazyLock, Mutex, MutexGuard};

use super::{input_mode::InputMode, ipc_service::IPCService};

/// State shared across every UI thread of the host process.
///
/// Only genuinely process-wide state lives here: the IPC connection (one
/// server session per host process) and the input mode. Per-activation COM
/// state — sink cookies and the layout-sink context — lives in the
/// per-instance `TextService` instead: TSF activates one TIP per UI thread,
/// and sharing those here made thread B overwrite thread A's cookie and let
/// one thread call another thread's `ITfContext` across apartments (B14).
/// Everything stored here is naturally Send + Sync; do NOT add COM interface
/// pointers (or `unsafe impl Send/Sync` to smuggle them past the compiler).
#[derive(Debug)]
pub struct IMEState {
    pub ipc_service: Option<IPCService>,
    pub input_mode: InputMode,
}

pub static IME_STATE: LazyLock<Mutex<IMEState>> = LazyLock::new(|| {
    tracing::debug!("Creating IMEState");
    Mutex::new(IMEState {
        ipc_service: None,
        input_mode: InputMode::default(),
    })
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
