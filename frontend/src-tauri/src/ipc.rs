use anyhow::Result;
use shared::proto::azookey_service_client::AzookeyServiceClient;
use std::{sync::Arc, time::Duration};
use tokio::time;

/// Upper bound for a single request to the server, including the lazy
/// connection attempt — a missing or busy server must not hang the
/// settings app.
const RPC_TIMEOUT: Duration = Duration::from_secs(3);

// connect to kkc server
#[derive(Debug, Clone)]
pub struct IPCService {
    // kkc server client
    azookey_client: AzookeyServiceClient<tonic::transport::Channel>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl IPCService {
    pub fn new() -> Result<Self> {
        // The runtime, and the channel built inside its context — see
        // shared::pipe::blocking_channels. Tauri invokes commands on the main
        // thread, which has no ambient reactor, and the panic that would
        // cause cannot unwind across the WebView2 COM callback: it aborts the
        // settings app. This used to be a hand-copied version of the TIP's
        // constructor.
        let (runtime, server_channel) = shared::pipe::blocking_channels(|| {
            Ok::<_, anyhow::Error>(shared::pipe::lazy_pipe_channel(shared::pipe::server_pipe())?)
        })?;
        let azookey_client = AzookeyServiceClient::new(server_channel);

        Ok(Self {
            azookey_client,
            runtime: Arc::new(runtime),
        })
    }
}

/// Why an `update_config` call did not succeed, in the only distinction the
/// caller acts on: whether this channel is worth keeping.
pub enum NotifyFailure {
    /// The connection is gone. The channel is dropped so the next call builds
    /// a fresh one.
    ConnectionLost(String),
    /// The server was reached and did not finish the job — it timed out, or
    /// it answered with an error. The channel is fine and stays.
    Rejected(String),
}

impl NotifyFailure {
    pub fn message(&self) -> &str {
        match self {
            NotifyFailure::ConnectionLost(message) | NotifyFailure::Rejected(message) => message,
        }
    }

    pub fn connection_lost(&self) -> bool {
        matches!(self, NotifyFailure::ConnectionLost(_))
    }
}

// implement methods to interact with kkc server
impl IPCService {
    /// Tells the server to re-read the settings file.
    ///
    /// The failure is classified because the caller does something different
    /// with each: everything used to be treated as a dead connection, so a
    /// server merely being SLOW — the engine is single-threaded, and a
    /// conversion in flight holds it — cost the channel. The next keystroke
    /// in a settings text field then rebuilt a whole tokio runtime, which is
    /// the expensive half of the freeze that was blamed on the timeout.
    /// `crates/shared/src/pipe.rs` makes the same distinction for the TIP and
    /// says why the timeout is not evidence of a lost connection.
    pub fn update_config(&mut self) -> Result<(), NotifyFailure> {
        let mut client = self.azookey_client.clone();
        self.runtime.block_on(async move {
            let request = tonic::Request::new(shared::proto::UpdateConfigRequest {});
            match time::timeout(RPC_TIMEOUT, client.update_config(request)).await {
                Err(_) => Err(NotifyFailure::Rejected(
                    "the IME did not answer in time; it will pick the settings up when it \
                     next starts"
                        .to_string(),
                )),
                Ok(Ok(_)) => Ok(()),
                Ok(Err(status)) => {
                    let message = status.message().to_string();
                    if shared::pipe::is_transport_failure(&status) {
                        Err(NotifyFailure::ConnectionLost(format!(
                            "cannot reach the IME ({message}); it will pick the settings up \
                             when it next starts"
                        )))
                    } else {
                        Err(NotifyFailure::Rejected(format!(
                            "the IME refused the settings: {message}"
                        )))
                    }
                }
            }
        })
    }

    /// Tells the server to forget everything it has learned.
    ///
    /// Same timeout and same failure classification as `update_config`, for
    /// the same reasons — but this one saves nothing first, so a failure is
    /// the whole outcome rather than a footnote to one: nothing has been
    /// reset, and the user has to be told that plainly.
    pub fn reset_learning(&mut self) -> Result<(), NotifyFailure> {
        let mut client = self.azookey_client.clone();
        self.runtime.block_on(async move {
            let request = tonic::Request::new(shared::proto::ResetLearningRequest {});
            match time::timeout(RPC_TIMEOUT, client.reset_learning(request)).await {
                Err(_) => Err(NotifyFailure::Rejected(
                    "the IME did not answer in time; nothing was reset".to_string(),
                )),
                Ok(Ok(_)) => Ok(()),
                Ok(Err(status)) => {
                    let message = status.message().to_string();
                    if shared::pipe::is_transport_failure(&status) {
                        Err(NotifyFailure::ConnectionLost(format!(
                            "cannot reach the IME ({message}); nothing was reset"
                        )))
                    } else {
                        Err(NotifyFailure::Rejected(format!(
                            "the IME could not reset the learning history: {message}"
                        )))
                    }
                }
            }
        })
    }
}
