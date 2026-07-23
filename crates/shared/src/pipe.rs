//! Shared tonic-over-named-pipe client connector, used by the TSF client,
//! the settings app, and the launcher's watchdog.
//!
//! Pipe names embed the terminal-services session id: named pipes live in a
//! machine-global namespace, so on RDP / fast user switching a fixed name
//! made the second user's IME silently talk to (or fight over) the first
//! user's engine (B19). Every process of one session — client DLL, server,
//! ui, launcher, settings app — derives the same name from its own session
//! id, so the whole stack stays session-local.

use std::os::windows::io::RawHandle;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::{net::windows::named_pipe::NamedPipeClient, time};
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;
use windows::Win32::{
    Foundation::ERROR_PIPE_BUSY,
    Storage::FileSystem::{
        CreateFileW, FILE_APPEND_DATA, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_NONE, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::RemoteDesktop::ProcessIdToSessionId,
    System::Threading::GetCurrentProcessId,
};
use windows::core::PCWSTR;

/// Access mask we open the client end with: FILE_GENERIC_READ |
/// FILE_GENERIC_WRITE, but WITHOUT FILE_APPEND_DATA. On a named pipe
/// FILE_APPEND_DATA (0x4) is the *same* bit as FILE_CREATE_PIPE_INSTANCE, so a
/// client opened with plain GENERIC_WRITE (what tokio's ClientOptions requests)
/// is implicitly granted the right to create rogue instances of the pipe. By
/// opening without that bit, the server's DACL can withhold create-instance
/// from sandboxed principals (AppContainer, restricted tokens) while they can
/// still connect. Everything a byte-mode client needs to read and write is
/// retained. See [`open_pipe_client`] and the server's pipe DACL.
const PIPE_CLIENT_ACCESS: u32 = (FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0) & !FILE_APPEND_DATA.0;

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
                        // SAFETY: called inside the tonic/tokio runtime, which
                        // NamedPipeClient::from_raw_handle requires.
                        match unsafe { open_pipe_client(&pipe_name) } {
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

/// True when `status` says the peer process could not be reached, rather than
/// carrying an answer the peer produced. Callers that keep state mirrored in
/// the peer (the TIP's composition) use this to tell "the server lost my
/// state, reset" from "the server rejected this request, surface it".
///
/// A dead engine surfaces in **two** different shapes, and both must count:
///
/// * the connect attempt fails (the pipe is gone) — tonic wraps every custom
///   connector's error in `tonic::ConnectError` and its source-chain
///   classifier maps that to `Unavailable`;
/// * an already-established connection breaks mid-call — hyper reports a
///   broken pipe, which tonic's classifier does *not* recognise, so the call
///   comes back as **`Unknown`** with a `tonic::transport::Error` source.
///
/// The second shape is the common one in the field: the channel connected at
/// the first keystroke and the engine died later, so the next RPC goes out on
/// the stale connection. Matching on `Unavailable` alone therefore misses the
/// most frequent server-loss case. A status the server itself produced (an
/// `InvalidArgument` decoded from trailers) never carries a transport error in
/// its source chain, so this stays precise.
pub fn is_transport_failure(status: &tonic::Status) -> bool {
    if status.code() == tonic::Code::Unavailable {
        return true;
    }

    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(status);
    while let Some(error) = source {
        if error.is::<tonic::transport::Error>() {
            return true;
        }
        source = error.source();
    }

    false
}

/// Opens the client end of `pipe_name` the way tokio's `ClientOptions::open`
/// does — OPEN_EXISTING, overlapped, identification-level SQOS — but with
/// [`PIPE_CLIENT_ACCESS`] instead of GENERIC_READ|GENERIC_WRITE, so the client
/// never requests (and the server's DACL need never grant it)
/// FILE_CREATE_PIPE_INSTANCE. The pipe is byte-mode, so no
/// SetNamedPipeHandleState is needed.
///
/// # Safety
/// Must run within a Tokio runtime; `NamedPipeClient::from_raw_handle`
/// registers the handle with the reactor. The handle is opened for overlapped
/// I/O and its ownership is transferred to the returned client, which closes
/// it on drop.
unsafe fn open_pipe_client(pipe_name: &str) -> std::io::Result<NamedPipeClient> {
    unsafe {
        let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();

        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            PIPE_CLIENT_ACCESS,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            // no template file; windows 0.62 models this parameter as an
            // Option rather than the null handle it used to take
            None,
        )
        // windows returns HRESULT_FROM_WIN32; recover the Win32 code so the
        // caller's ERROR_PIPE_BUSY retry (raw_os_error) keeps working
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;

        NamedPipeClient::from_raw_handle(handle.0 as RawHandle)
    }
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

    /// A status the server produced is an answer, not a transport failure:
    /// classifying it as server loss would reset the composition on every
    /// rejected request.
    #[test]
    fn a_server_side_status_is_not_a_transport_failure() {
        assert!(!is_transport_failure(&tonic::Status::invalid_argument(
            "bad request"
        )));
        assert!(!is_transport_failure(&tonic::Status::internal(
            "engine blew up"
        )));
    }

    /// The connect-failure shape: tonic maps a failing connector to
    /// `Unavailable`. (The other shape — `Unknown` over a broken established
    /// connection — needs a real connection to break, so it is covered by
    /// `crates/server/tests/pipe_transport.rs`.)
    #[test]
    fn unavailable_is_a_transport_failure() {
        assert!(is_transport_failure(&tonic::Status::unavailable(
            "pipe is gone"
        )));
    }

    /// The client access mask must let a byte-mode client read and write, but
    /// must NOT include FILE_APPEND_DATA — which on a pipe is the same bit as
    /// FILE_CREATE_PIPE_INSTANCE. If it did, sandboxed clients granted "write"
    /// would also be able to create rogue pipe instances and hijack a peer's
    /// connection (keystroke interception).
    #[test]
    fn client_access_mask_cannot_create_pipe_instances() {
        assert_eq!(
            PIPE_CLIENT_ACCESS & FILE_APPEND_DATA.0,
            0,
            "the client must not request FILE_CREATE_PIPE_INSTANCE"
        );
        // still a working read/write client
        assert_ne!(PIPE_CLIENT_ACCESS & FILE_GENERIC_READ.0, 0);
        assert_ne!(
            PIPE_CLIENT_ACCESS & 0x0002,
            0,
            "FILE_WRITE_DATA is retained"
        );
        // exactly (FILE_GENERIC_READ | FILE_GENERIC_WRITE) minus append
        assert_eq!(PIPE_CLIENT_ACCESS, 0x0012_019B);
    }
}
