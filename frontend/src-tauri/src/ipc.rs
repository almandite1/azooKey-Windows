use anyhow::Result;
use shared::proto::azookey_service_client::AzookeyServiceClient;
use std::time::Duration;
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
}

impl IPCService {
    /// MUST be called from inside a Tokio runtime — every caller is a
    /// `#[tauri::command]`, which is exactly that.
    ///
    /// This used to build a runtime of its OWN and drive each call with
    /// `Runtime::block_on`, because commands were synchronous and the main
    /// thread has no ambient reactor. Once they became `async fn` that
    /// stopped being merely redundant and started being fatal: `block_on`
    /// from a thread already driving a runtime panics ("Cannot start a
    /// runtime from within a runtime"), the panic kills the spawned task
    /// rather than returning, and the `invoke` promise on the other side
    /// NEVER settles. The settings app's own button sat disabled forever
    /// with nothing on screen to say why.
    ///
    /// So: no runtime here, and the calls below are `async` and awaited by
    /// the command. The channel is lazy, so nothing connects until the first
    /// request — which is why this can be built before the IME is running.
    pub fn new() -> Result<Self> {
        let server_channel = shared::pipe::lazy_pipe_channel(shared::pipe::server_pipe())?;
        Ok(Self {
            azookey_client: AzookeyServiceClient::new(server_channel),
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
    pub async fn update_config(&self) -> Result<(), NotifyFailure> {
        let mut client = self.azookey_client.clone();
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
    }

    /// Tells the server to forget everything it has learned.
    ///
    /// Same timeout and same failure classification as `update_config`, for
    /// the same reasons — but this one saves nothing first, so a failure is
    /// the whole outcome rather than a footnote to one: nothing has been
    /// reset, and the user has to be told that plainly.
    pub async fn reset_learning(&self) -> Result<(), NotifyFailure> {
        let mut client = self.azookey_client.clone();
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
    }
}

/// Live checks against a RUNNING azookey-server, excluded from `cargo test`
/// by `#[ignore]` for the same reason as `crates/server/tests/ipc_smoke.rs`.
///
///     cargo test -p azookey --lib -- --ignored --test-threads=1 --nocapture
///
/// They exist because the settings app's two server calls are otherwise only
/// exercised by clicking, and "the button does nothing" is indistinguishable
/// from a hang at this layer without them.
#[cfg(test)]
mod tests {
    use super::{IPCService, NotifyFailure, RPC_TIMEOUT};

    /// Times one call from INSIDE a runtime, which is what a
    /// `#[tauri::command]` is. That detail is the point: these calls are only
    /// correct when awaited on somebody else's runtime, and the shape that
    /// broke -- driving them with a private `Runtime::block_on` -- is exactly
    /// what a test that built its own runtime and blocked on the call would
    /// reproduce instead.
    fn timed<F, Fut>(call: F) -> std::time::Duration
    where
        F: FnOnce(IPCService) -> Fut,
        Fut: std::future::Future<Output = Result<(), NotifyFailure>>,
    {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime for the test");
        runtime.block_on(async {
            let service = IPCService::new().expect("connect to the server");
            let start = std::time::Instant::now();
            let outcome = call(service).await;
            let elapsed = start.elapsed();

            assert!(
                elapsed < RPC_TIMEOUT * 2,
                "the call took {elapsed:?}, past its own ceiling"
            );
            if let Err(failure) = outcome {
                panic!("the call failed: {}", failure.message());
            }
            elapsed
        })
    }

    /// The reset is one RPC with a 3s ceiling, so a call that takes longer
    /// than that has hung somewhere the timeout does not cover.
    #[test]
    #[ignore = "requires a running azookey-server; RESETS the learning history"]
    fn reset_learning_answers_within_its_timeout() {
        let elapsed = timed(|service| async move { service.reset_learning().await });
        println!("reset_learning {elapsed:?}");
    }

    /// The call the settings app already made before this feature existed --
    /// the control for the one above.
    #[test]
    #[ignore = "requires a running azookey-server"]
    fn update_config_answers_within_its_timeout() {
        let elapsed = timed(|service| async move { service.update_config().await });
        println!("update_config {elapsed:?}");
    }
}
