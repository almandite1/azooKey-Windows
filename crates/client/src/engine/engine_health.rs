//! Whether the conversion engine is answering, kept process-wide so the
//! language bar can say so out loud (issue #79).
//!
//! When the logon task does not fire, `launcher.exe` — and with it
//! `azookey-server.exe` and `ui.exe` — never starts. The TIP itself is
//! unaffected: it loads, activates, draws its language-bar icon, and switches
//! between あ and A, because the input mode lives in TSF's compartments
//! rather than in the engine. So the *only* symptom the user sees is that
//! nothing converts, which looks exactly like a conversion bug and cost real
//! time to tell apart once already.
//!
//! The language-bar tooltip is the one surface the TIP can reach on its own.
//! `ui.exe` is down in precisely this scenario, so the mode indicator cannot
//! be the messenger, and a notification or message box from inside a TIP
//! would fire in every text application that has the DLL loaded.

use std::sync::atomic::{AtomicU8, Ordering};

/// What the most recent engine RPC said about the conversion server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineHealth {
    /// No engine RPC has been attempted yet. `Activate` probes the server as
    /// soon as the TIP comes up, so this is a brief startup state.
    Unknown,
    /// The last attempt was answered. A server-side *rejection* counts as
    /// reachable too: the reply travelled over a working pipe, so the engine
    /// is alive and the fault is in the request.
    Reachable,
    /// The last attempt failed with [`super::ipc_service::ServerUnavailable`]
    /// — the pipe is gone (no server process) or it did not answer in time.
    Unreachable,
}

impl EngineHealth {
    fn from_repr(repr: u8) -> Self {
        match repr {
            REACHABLE => Self::Reachable,
            UNREACHABLE => Self::Unreachable,
            // any other value cannot occur: `store` is only ever fed one of
            // the three constants below
            _ => Self::Unknown,
        }
    }

    fn repr(self) -> u8 {
        match self {
            Self::Unknown => UNKNOWN,
            Self::Reachable => REACHABLE,
            Self::Unreachable => UNREACHABLE,
        }
    }
}

const UNKNOWN: u8 = 0;
const REACHABLE: u8 = 1;
const UNREACHABLE: u8 = 2;

/// Process-wide, not per-activation: one host process holds one connection to
/// the server (see [`super::state::IMEState`]), so every UI thread's language
/// bar should report the same verdict. An atomic rather than a field on
/// `IMEState` because `GetTooltipString` is a TSF callback that may arrive
/// while another callback holds that mutex, and `IMEState::get` fails on
/// re-entrancy — a tooltip must not be the thing that fails.
static HEALTH: AtomicU8 = AtomicU8::new(UNKNOWN);

/// Records what one engine RPC's outcome says about the server. Called for
/// every engine RPC that reaches the wire, so it must stay this cheap.
///
/// `IPCService`'s test fake short-circuits *before* `engine_exec`, so a
/// scripted `engine_unavailable` does not reach here. That is deliberate:
/// `handle_action`'s golden tests would otherwise write this process-wide
/// flag and become order-dependent on each other.
pub fn record<T>(result: &anyhow::Result<T>) {
    let health = match result {
        Ok(_) => EngineHealth::Reachable,
        Err(error) if super::ipc_service::is_server_unavailable(error) => EngineHealth::Unreachable,
        // a rejection is still an answer
        Err(_) => EngineHealth::Reachable,
    };
    set(health);
}

/// Publishes `health`, logging the transitions. `ERROR`, not `WARN`: when the
/// log is the only evidence available, "the engine is not there" is what
/// should catch the eye first (issue #79).
pub fn set(health: EngineHealth) {
    let previous = EngineHealth::from_repr(HEALTH.swap(health.repr(), Ordering::Relaxed));
    if previous == health {
        return;
    }
    match health {
        EngineHealth::Unreachable => tracing::error!(
            "the azooKey conversion engine is unreachable: nothing will convert until \
             launcher.exe is running again (previous state: {previous:?})"
        ),
        EngineHealth::Reachable => tracing::info!("the azooKey conversion engine is answering"),
        EngineHealth::Unknown => {}
    }
}

/// The current verdict, for the language bar to render.
pub fn get() -> EngineHealth {
    EngineHealth::from_repr(HEALTH.load(Ordering::Relaxed))
}

/// The tooltip text for the language-bar item under `health`. Returns `None`
/// when there is nothing worth saying — a working engine needs no tooltip,
/// and neither does a state we have not established yet.
///
/// Japanese, like every other user-facing string the installer and the
/// settings app use, and it names `launcher.exe` because that is the thing
/// the user has to start.
pub fn tooltip(health: EngineHealth) -> Option<&'static str> {
    match health {
        EngineHealth::Unreachable => Some(
            "azooKey: 変換エンジンに接続できません。\
             launcher.exe が起動しているか確認してください（IME 自体は動作しています）。",
        ),
        EngineHealth::Reachable | EngineHealth::Unknown => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::ipc_service::ServerUnavailable;
    use crate::tsf::test_support::global_state_lock;

    #[test]
    fn every_state_survives_a_round_trip_through_the_atomic() {
        let _serialize = global_state_lock();
        for health in [
            EngineHealth::Unknown,
            EngineHealth::Reachable,
            EngineHealth::Unreachable,
        ] {
            set(health);
            assert_eq!(get(), health);
        }
    }

    /// The transport tag is what marks the engine absent — the ONE outcome
    /// that must flip the flag.
    #[test]
    fn a_transport_failure_marks_the_engine_unreachable() {
        let _serialize = global_state_lock();
        set(EngineHealth::Reachable);

        let result: anyhow::Result<()> =
            Err(anyhow::Error::new(ServerUnavailable).context("IPC transport error"));
        record(&result);

        assert_eq!(get(), EngineHealth::Unreachable);
    }

    /// A rejection travelled over a working pipe, so the engine is alive.
    /// Treating it as "unreachable" would tell the user to restart the
    /// launcher over what is really a bad request.
    #[test]
    fn a_rejected_request_still_counts_as_reachable() {
        let _serialize = global_state_lock();
        set(EngineHealth::Unreachable);

        let result: anyhow::Result<()> = Err(anyhow::anyhow!("InvalidArgument"));
        record(&result);

        assert_eq!(get(), EngineHealth::Reachable);
    }

    #[test]
    fn a_successful_rpc_clears_an_earlier_verdict() {
        let _serialize = global_state_lock();
        set(EngineHealth::Unreachable);

        record(&anyhow::Ok(()));

        assert_eq!(get(), EngineHealth::Reachable);
    }

    /// Only the unreachable state has something to say; a tooltip on a
    /// healthy engine would be noise on every hover.
    #[test]
    fn only_an_unreachable_engine_produces_a_tooltip() {
        assert!(tooltip(EngineHealth::Unreachable).is_some());
        assert_eq!(tooltip(EngineHealth::Reachable), None);
        assert_eq!(tooltip(EngineHealth::Unknown), None);
    }

    /// The tooltip has to name the process the user must start, or it says
    /// "broken" without saying what to do about it.
    #[test]
    fn the_tooltip_names_the_launcher() {
        let text = tooltip(EngineHealth::Unreachable).unwrap();
        assert!(text.contains("launcher.exe"), "{text}");
    }
}
