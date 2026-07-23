use async_stream::stream;
use futures_core::stream::Stream;
use std::sync::atomic::{AtomicI64, Ordering};
use std::{ffi::c_void, pin::Pin, ptr::addr_of_mut};
use tokio::sync::mpsc;
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
};
use tonic::transport::server::Connected;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{HSTRING, PCWSTR, PWSTR},
};

// repr(transparent): the pipe-creation calls pass `&mut security_attributes
// as *mut c_void`, which relies on the wrapper having its single field's
// exact layout at offset 0 — repr(Rust) does not guarantee that
#[repr(transparent)]
#[allow(dead_code)]
struct UnsafeSecurityAttributes(SECURITY_ATTRIBUTES);

unsafe impl Send for UnsafeSecurityAttributes {}
unsafe impl Sync for UnsafeSecurityAttributes {}

pub struct TonicNamedPipeServer {
    inner: NamedPipeServer,
    session_id: i64,
    /// Told which session ended when this connection is dropped. `None` for
    /// servers built with [`TonicNamedPipeServer::new`], which is every
    /// server that does not care (the conversion server evicts idle sessions
    /// on a timer instead).
    disconnect_tx: Option<mpsc::UnboundedSender<i64>>,
}

/// tonic drops the connection's I/O object when the connection ends, which
/// includes the client process dying — the only signal a server gets that an
/// application is gone. Unbounded because a `Drop` cannot await.
impl Drop for TonicNamedPipeServer {
    fn drop(&mut self) {
        if let Some(tx) = &self.disconnect_tx {
            // the receiver is gone during shutdown; nothing to do about it
            let _ = tx.send(self.session_id);
        }
    }
}

/// Identifies one accepted pipe connection. tonic clones this into every
/// request's extensions, which lets handlers keep per-client (per-IME-
/// instance) composing state instead of one global shared by all apps.
#[derive(Debug, Clone, Copy)]
pub struct PipeConnectInfo {
    pub session_id: i64,
}

impl Connected for TonicNamedPipeServer {
    type ConnectInfo = PipeConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        PipeConnectInfo {
            session_id: self.session_id,
        }
    }
}

impl AsyncRead for TonicNamedPipeServer {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TonicNamedPipeServer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// SID string (e.g. `S-1-5-21-…`) of the user running this process. Used to
/// scope the pipe DACL to the owning user instead of the whole Builtin Users
/// group, so a different local user (RDP / fast user switching) cannot reach
/// this session's pipe even though the pipe name is predictable.
fn current_user_sid_string() -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| io::Error::other(format!("OpenProcessToken failed: {e}")))?;

        // Query TokenUser, then look up the SID, always closing the token.
        let result = (|| {
            let mut needed = 0u32;
            // First call sizes the buffer; it is expected to fail with
            // ERROR_INSUFFICIENT_BUFFER while writing the required length.
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::other("GetTokenInformation returned zero length"));
            }

            let mut buffer = vec![0u8; needed as usize];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr() as *mut c_void),
                needed,
                &mut needed,
            )
            .map_err(|e| io::Error::other(format!("GetTokenInformation failed: {e}")))?;

            let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut sid_string = PWSTR::null();
            ConvertSidToStringSidW(token_user.User.Sid, &mut sid_string)
                .map_err(|e| io::Error::other(format!("ConvertSidToStringSidW failed: {e}")))?;

            let owned = sid_string
                .to_string()
                .map_err(|e| io::Error::other(format!("SID string was not valid UTF-16: {e}")));
            // ConvertSidToStringSidW allocates with LocalAlloc; free it whether
            // or not the UTF-16 decode succeeded.
            let _ = LocalFree(Some(HLOCAL(sid_string.0 as *mut c_void)));
            owned
        })();

        let _ = CloseHandle(token);
        result
    }
}

/// The pipe's security descriptor, in SDDL, for the owning user's SID.
///
/// DACL: sandboxed principals may CONNECT but not CREATE pipe instances.
/// GENERIC_ALL (GA) includes FILE_CREATE_PIPE_INSTANCE, so the original
/// all-GA descriptor let any AppContainer (AC) or restricted (RC) process
/// stand up a rogue instance of this pipe and receive a peer client's
/// connection — intercepting the raw keystroke stream (AppendText). AC/RC get
/// 0x12019B (FILE_GENERIC_READ|FILE_GENERIC_WRITE minus FILE_APPEND_DATA ==
/// minus FILE_CREATE_PIPE_INSTANCE), which is exactly what the client opens
/// with (see `shared::pipe::PIPE_CLIENT_ACCESS`), so connecting still works.
/// The owning user's SID (not the whole BU group) keeps GA: the server runs as
/// that user and must create instances, while a *different* local user — whose
/// token lacks this SID — is no longer granted access, closing the RDP / fast-
/// user-switching cross-session hole. SY/BA keep GA for SYSTEM and an elevated
/// server. SACL: low-IL clients (sandboxed browsers) may still write up to the
/// pipe.
///
/// See https://nathancorvussolis.blogspot.com/2018/05/windows-ime-security.html
fn pipe_sddl(user_sid: &str) -> String {
    format!(
        "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})(A;;0x12019b;;;AC)(A;;0x12019b;;;RC)S:(ML;;NW;;;LW)"
    )
}

impl TonicNamedPipeServer {
    /// Accepts connections on `path`, with no interest in when they end.
    pub fn new(
        path: &str,
    ) -> io::Result<impl Stream<Item = io::Result<TonicNamedPipeServer>> + use<>> {
        Self::build(path, None)
    }

    /// Accepts connections on `path` and sends the session id of each one
    /// down `disconnect_tx` when it ends. For state that has to be undone
    /// when a client goes away rather than aged out — the candidate window
    /// is only ever hidden by an RPC, so a host application killed
    /// mid-composition used to leave it on screen forever.
    pub fn with_disconnect_notify(
        path: &str,
        disconnect_tx: mpsc::UnboundedSender<i64>,
    ) -> io::Result<impl Stream<Item = io::Result<TonicNamedPipeServer>> + use<>> {
        Self::build(path, Some(disconnect_tx))
    }

    fn build(
        path: &str,
        disconnect_tx: Option<mpsc::UnboundedSender<i64>>,
    ) -> io::Result<impl Stream<Item = io::Result<TonicNamedPipeServer>> + use<>> {
        // security attributes that let sandboxed processes do IPC without
        // being able to hijack the pipe — see `pipe_sddl`

        let name = format!("\\\\.\\pipe\\{}", path);

        let mut security_descriptor = PSECURITY_DESCRIPTOR::default();

        let sddl = HSTRING::from(pipe_sddl(&current_user_sid_string()?));

        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION,
                &mut security_descriptor,
                None,
            )
            .map_err(|e| io::Error::other(format!("invalid pipe security descriptor: {e}")))?;

            let mut security_attributes = UnsafeSecurityAttributes(SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: security_descriptor.0,
                bInheritHandle: false.into(),
            });

            Ok(stream! {
                // startup failure is fatal (e.g. another instance already
                // owns the pipe), but once serving, transient accept or
                // instance-creation errors must not tear down the whole
                // gRPC server — log and retry instead of yielding Err
                let mut server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create_with_security_attributes_raw(
                        &name,
                        addr_of_mut!(security_attributes) as *mut c_void
                    )?;

                static NEXT_SESSION_ID: AtomicI64 = AtomicI64::new(1);

                loop {
                    // the accept and create results below are bound to locals
                    // rather than matched as temporaries: a scrutinee temporary
                    // holding a pipe handle changes drop point between editions
                    // 2021 and 2024, and this loop must not depend on which
                    let accepted = server.connect().await;
                    match accepted {
                        Ok(()) => {
                            yield Ok(TonicNamedPipeServer {
                                inner: server,
                                session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
                                disconnect_tx: disconnect_tx.clone(),
                            });
                        }
                        Err(e) => {
                            tracing::warn!("named pipe accept failed: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }

                    server = loop {
                        let created = ServerOptions::new().create_with_security_attributes_raw(
                            &name,
                            addr_of_mut!(security_attributes) as *mut c_void,
                        );
                        match created {
                            Ok(s) => break s,
                            Err(e) => {
                                tracing::warn!("failed to create next pipe instance: {e}");
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                    };
                }
            })
        }
    }
}

/// The pipe DACL is a security boundary: every application on the desktop can
/// open this pipe, and what it may do there is decided entirely by this
/// string. The descriptor itself is only checked by Windows at pipe-creation
/// time, so a typo would show up as "the IME still works" — with the
/// hardening silently gone.
#[cfg(test)]
mod tests {
    use super::pipe_sddl;

    const USER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    /// The owning user's own SID, not the Builtin Users group: a *different*
    /// local user (RDP, fast user switching) must not be granted anything,
    /// even though the pipe name is predictable.
    #[test]
    fn the_owning_user_is_granted_full_access_by_sid() {
        let sddl = pipe_sddl(USER);

        assert!(
            sddl.contains(&format!("(A;;GA;;;{USER})")),
            "the user's SID must be embedded verbatim: {sddl}"
        );
        assert!(!sddl.contains(";;;BU)"), "the Users group must not appear");
    }

    /// The hardening itself: sandboxed principals get the client's access
    /// mask, never GENERIC_ALL. GA would include FILE_CREATE_PIPE_INSTANCE
    /// and let an AppContainer stand up a rogue instance of this pipe and
    /// receive a peer's keystrokes.
    #[test]
    fn sandboxed_principals_cannot_create_pipe_instances() {
        let sddl = pipe_sddl(USER);

        assert!(sddl.contains("(A;;0x12019b;;;AC)"), "{sddl}");
        assert!(sddl.contains("(A;;0x12019b;;;RC)"), "{sddl}");
        assert!(
            !sddl.contains("(A;;GA;;;AC)") && !sddl.contains("(A;;GA;;;RC)"),
            "no sandboxed principal may hold GENERIC_ALL: {sddl}"
        );
    }

    /// The granted mask must be exactly the one the client opens with, or
    /// connecting breaks. `shared::pipe`'s own test pins
    /// `PIPE_CLIENT_ACCESS == 0x0012_019B`; this is the other end of that
    /// pairing, and the two literals must be changed together.
    #[test]
    fn the_sandboxed_mask_is_the_one_the_client_opens_with() {
        // the value `shared::pipe::PIPE_CLIENT_ACCESS` computes and its own
        // test pins
        const CLIENT_ACCESS: u32 = 0x0012_019B;
        let sddl = pipe_sddl(USER);

        assert_eq!(
            sddl.matches(&format!("{CLIENT_ACCESS:#x}")).count(),
            2,
            "both sandboxed principals carry exactly the client's mask: {sddl}"
        );
    }

    /// SYSTEM and the Administrators group keep GENERIC_ALL (an elevated
    /// server must be able to create instances), and the SACL keeps low-IL
    /// clients — sandboxed browsers — able to write up to the pipe. Losing
    /// the mandatory label would break input in every sandboxed host.
    #[test]
    fn system_administrators_and_low_integrity_clients_keep_their_access() {
        let sddl = pipe_sddl(USER);

        assert!(sddl.contains("(A;;GA;;;SY)"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;BA)"), "{sddl}");
        assert!(sddl.ends_with("S:(ML;;NW;;;LW)"), "{sddl}");
    }
}
