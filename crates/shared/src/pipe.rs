//! Shared tonic-over-named-pipe client connector, used by the TSF client,
//! the settings app, and the launcher's watchdog.

use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::{net::windows::named_pipe::ClientOptions, time};
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;
use windows::Win32::Foundation::ERROR_PIPE_BUSY;

/// Pipe name of the kana-kanji conversion server.
pub const SERVER_PIPE: &str = r"\\.\pipe\azookey_server";
/// Pipe name of the candidate window (UI) process.
pub const UI_PIPE: &str = r"\\.\pipe\azookey_ui";

/// Creates a lazy tonic channel over a named pipe.
///
/// No connection is attempted here: each RPC connects on demand and tonic
/// re-establishes the connection after transport failures, so callers
/// recover automatically when the peer process restarts. The busy-retry
/// loop below never gives up on its own — callers MUST bound every RPC
/// (and thereby the connection attempt) with a timeout.
pub fn lazy_pipe_channel(pipe_name: &'static str) -> Result<Channel, tonic::transport::Error> {
    // the URI is a placeholder; the connector below opens a named pipe
    Ok(
        Endpoint::try_from("http://[::]:50051")?.connect_with_connector_lazy(service_fn(
            move |_| async move {
                let client = loop {
                    match ClientOptions::new().open(pipe_name) {
                        Ok(client) => break client,
                        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => (),
                        Err(e) => return Err(e),
                    }

                    time::sleep(Duration::from_millis(50)).await;
                };

                Ok::<_, std::io::Error>(TokioIo::new(client))
            },
        )),
    )
}
