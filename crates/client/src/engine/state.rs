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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{IME_STATE, IMEState};
    use crate::tsf::test_support::global_state_lock;

    /// TSF dispatches callbacks re-entrantly (a compartment write calls
    /// `OnChange` synchronously from inside `SetValue`, for one). A blocking
    /// lock would deadlock the host's UI thread there, so a second borrow
    /// fails instead — and the caller's `?` turns it into an HRESULT.
    #[test]
    fn a_reentrant_borrow_fails_instead_of_deadlocking() {
        let _serialize = global_state_lock();
        let outer = IMEState::get().expect("the first borrow must succeed");

        assert!(
            IMEState::get().is_err(),
            "a second borrow must not block waiting for the first"
        );

        drop(outer);
        assert!(
            IMEState::get().is_ok(),
            "the lock is free again once the first borrow ends"
        );
    }

    /// A panic anywhere under the guard poisons the mutex. Refusing every
    /// later borrow would disable the IME for the rest of the host process's
    /// life — for state that is just an IPC handle, recovering is strictly
    /// better than going dead.
    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_fatal() {
        let _serialize = global_state_lock();

        // panic while holding the lock, on a thread of its own so this test
        // survives it. The panic message on stderr is expected output.
        let poisoner = std::thread::spawn(|| {
            let _held = IME_STATE.lock();
            panic!("poisoning IMEState on purpose");
        });
        assert!(poisoner.join().is_err(), "the thread must have panicked");
        assert!(IME_STATE.is_poisoned());

        assert!(
            IMEState::get().is_ok(),
            "a poisoned IMEState must still be usable"
        );
    }
}
