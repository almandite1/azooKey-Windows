use async_stream::stream;
use futures_core::stream::Stream;
use std::{ffi::c_void, pin::Pin, ptr::addr_of_mut};
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
};
use tonic::transport::server::Connected;
use windows::{
    core::w,
    Win32::Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
};

#[allow(dead_code)]
struct UnsafeSecurityAttributes(SECURITY_ATTRIBUTES);

unsafe impl Send for UnsafeSecurityAttributes {}
unsafe impl Sync for UnsafeSecurityAttributes {}

pub struct TonicNamedPipeServer {
    inner: NamedPipeServer,
}

impl Connected for TonicNamedPipeServer {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {
        ()
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

impl TonicNamedPipeServer {
    pub fn new(
        path: &str,
    ) -> io::Result<impl Stream<Item = io::Result<TonicNamedPipeServer>>> {
        // set security attributes to allow ipc from sandboxed processes
        // see https://nathancorvussolis.blogspot.com/2018/05/windows-ime-security.html

        let name = format!("\\\\.\\pipe\\{}", path);

        let mut security_descriptor = PSECURITY_DESCRIPTOR::default();

        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:(A;;GA;;;AC)(A;;GA;;;RC)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;BU)S:(ML;;NW;;;LW)"),
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

                loop {
                    match server.connect().await {
                        Ok(()) => {
                            yield Ok(TonicNamedPipeServer { inner: server });
                        }
                        Err(e) => {
                            eprintln!("named pipe accept failed: {e}");
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
                                eprintln!("failed to create next pipe instance: {e}");
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                    };
                }
            })
        }
    }
}
