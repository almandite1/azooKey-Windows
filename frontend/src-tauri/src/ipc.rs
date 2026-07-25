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

// implement methods to interact with kkc server
impl IPCService {
    /// NOTE: unlike the TIP, this does not classify the failure with
    /// `shared::pipe::is_transport_failure` — every error, timeout or
    /// rejection alike, is reported to the settings UI the same way. That is
    /// deliberate for now: the settings app has one RPC and nothing to
    /// recover, so telling "the server is gone" from "the server said no"
    /// would change what the user is shown without changing what they can do
    /// about it. Revisit together with the settings app's error surface.
    pub fn update_config(&mut self) -> anyhow::Result<()> {
        let mut client = self.azookey_client.clone();
        self.runtime.block_on(async move {
            let request = tonic::Request::new(shared::proto::UpdateConfigRequest {});
            time::timeout(RPC_TIMEOUT, client.update_config(request))
                .await
                .map_err(|_| anyhow::anyhow!("request to azookey server timed out"))?
                .map_err(anyhow::Error::from)
        })?;

        Ok(())
    }
}
