//! Shared tonic-over-named-pipe client connector, used by the TSF client,
//! the settings app, and the launcher's watchdog.
//!
//! Pipe names embed the terminal-services session id: named pipes live in a
//! machine-global namespace, so on RDP / fast user switching a fixed name
//! made the second user's IME silently talk to (or fight over) the first
//! user's engine (B19). Every process of one session — client DLL, server,
//! ui, launcher, settings app — derives the same name from its own session
//! id, so the whole stack stays session-local.

use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::{net::windows::named_pipe::ClientOptions, time};
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;
use windows::Win32::{
    Foundation::ERROR_PIPE_BUSY, System::RemoteDesktop::ProcessIdToSessionId,
    System::Threading::GetCurrentProcessId,
};

/// Base name (no `\\.\pipe\` prefix) of the kana-kanji conversion server's
/// pipe for this session. This is what the server passes to its pipe
/// listener.
pub fn server_pipe_base() -> String {
    base_for("azookey_server", current_session_id())
}

/// Base name of the candidate window (UI) process's pipe for this session.
pub fn ui_pipe_base() -> String {
    base_for("azookey_ui", current_session_id())
}

/// Full pipe path of the conversion server for this session (client side).
pub fn server_pipe() -> String {
    format!(r"\\.\pipe\{}", server_pipe_base())
}

/// Full pipe path of the UI process for this session (client side).
pub fn ui_pipe() -> String {
    format!(r"\\.\pipe\{}", ui_pipe_base())
}

fn base_for(name: &str, session_id: u32) -> String {
    format!("{name}_{session_id}")
}

fn current_session_id() -> u32 {
    let mut session_id = 0u32;
    // Fails only for an invalid pid; our own pid is always valid. If it ever
    // fails anyway, 0 keeps every process of the session agreeing on a name.
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id);
    }
    session_id
}

/// Creates a lazy tonic channel over a named pipe.
///
/// No connection is attempted here: each RPC connects on demand and tonic
/// re-establishes the connection after transport failures, so callers
/// recover automatically when the peer process restarts. The busy-retry
/// loop below never gives up on its own — callers MUST bound every RPC
/// (and thereby the connection attempt) with a timeout.
pub fn lazy_pipe_channel(pipe_name: String) -> Result<Channel, tonic::transport::Error> {
    // the URI is a placeholder; the connector below opens a named pipe
    Ok(
        Endpoint::try_from("http://[::]:50051")?.connect_with_connector_lazy(service_fn(
            move |_| {
                let pipe_name = pipe_name.clone();
                async move {
                    let client = loop {
                        match ClientOptions::new().open(&pipe_name) {
                            Ok(client) => break client,
                            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => (),
                            Err(e) => return Err(e),
                        }

                        time::sleep(Duration::from_millis(50)).await;
                    };

                    Ok::<_, std::io::Error>(TokioIo::new(client))
                }
            },
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_embeds_the_session_id() {
        assert_eq!(base_for("azookey_server", 3), "azookey_server_3");
    }

    #[test]
    fn different_sessions_get_different_names() {
        assert_ne!(base_for("azookey_ui", 1), base_for("azookey_ui", 2));
    }

    #[test]
    fn full_paths_carry_the_pipe_prefix_and_session() {
        assert!(server_pipe().starts_with(r"\\.\pipe\azookey_server_"));
        assert!(ui_pipe().starts_with(r"\\.\pipe\azookey_ui_"));
        // base and full path must agree on the session id
        assert!(server_pipe().ends_with(&server_pipe_base()));
    }
}
