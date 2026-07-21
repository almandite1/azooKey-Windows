use async_stream::stream;
use futures_core::stream::Stream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::{ffi::c_void, pin::Pin, ptr::addr_of_mut};
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
};
use tonic::transport::server::Connected;
use windows::{
    core::{HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION,
            },
            GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
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
    session_id: i32,
}

/// Identifies one accepted pipe connection. tonic clones this into every
/// request's extensions, which lets handlers keep per-client (per-IME-
/// instance) composing state instead of one global shared by all apps.
#[derive(Debug, Clone, Copy)]
pub struct PipeConnectInfo {
    pub session_id: i32,
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
            let _ = LocalFree(HLOCAL(sid_string.0 as *mut c_void));
            owned
        })();

        let _ = CloseHandle(token);
        result
    }
}

impl TonicNamedPipeServer {
    pub fn new(path: &str) -> io::Result<impl Stream<Item = io::Result<TonicNamedPipeServer>>> {
        // set security attributes to allow ipc from sandboxed processes
        // see https://nathancorvussolis.blogspot.com/2018/05/windows-ime-security.html

        let name = format!("\\\\.\\pipe\\{}", path);

        let mut security_descriptor = PSECURITY_DESCRIPTOR::default();

        // DACL: sandboxed principals may CONNECT but not CREATE pipe instances.
        // GENERIC_ALL (GA) includes FILE_CREATE_PIPE_INSTANCE, so the original
        // all-GA descriptor let any AppContainer (AC) or restricted (RC)
        // process stand up a rogue instance of this pipe and receive a peer
        // client's connection — intercepting the raw keystroke stream
        // (AppendText). AC/RC get 0x12019B (FILE_GENERIC_READ|FILE_GENERIC_WRITE
        // minus FILE_APPEND_DATA == minus FILE_CREATE_PIPE_INSTANCE), which is
        // exactly what the client opens with (see shared::pipe::
        // PIPE_CLIENT_ACCESS), so connecting still works. The owning user's SID
        // (not the whole BU group) keeps GA: the server runs as that user and
        // must create instances, while a *different* local user — whose token
        // lacks this SID — is no longer granted access, closing the RDP / fast-
        // user-switching cross-session hole. SY/BA keep GA for SYSTEM and an
        // elevated server. SACL unchanged: low-IL clients (sandboxed browsers)
        // may still write up to the pipe.
        let user_sid = current_user_sid_string()?;
        let sddl = HSTRING::from(format!(
            "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})(A;;0x12019b;;;AC)(A;;0x12019b;;;RC)S:(ML;;NW;;;LW)"
        ));

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

                static NEXT_SESSION_ID: AtomicI32 = AtomicI32::new(1);

                loop {
                    match server.connect().await {
                        Ok(()) => {
                            yield Ok(TonicNamedPipeServer {
                                inner: server,
                                session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
                            });
                        }
                        Err(e) => {
                            tracing::warn!("named pipe accept failed: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }

                    server = loop {
                        match ServerOptions::new().create_with_security_attributes_raw(
                            &name,
                            addr_of_mut!(security_attributes) as *mut c_void,
                        ) {
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
