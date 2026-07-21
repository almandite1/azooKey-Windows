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
        let runtime = tokio::runtime::Runtime::new()?;

        // Enter the runtime context before building the channel: tonic's
        // connect_with_connector_lazy spawns the channel's connection task
        // and needs an ambient Tokio reactor, but Tauri invokes commands on
        // the main thread, which has none. Without this guard the spawn
        // panics ("there is no reactor running"), and the panic aborts the
        // whole settings app because it cannot unwind across the WebView2
        // COM callback that invoked the command. Same pattern as
        // crates/client/src/engine/ipc_service.rs.
        let _guard = runtime.enter();

        // lazy: no connection is attempted until the first RPC, and tonic
        // reconnects automatically after the server restarts
        let server_channel = shared::pipe::lazy_pipe_channel(shared::pipe::server_pipe())?;
        let azookey_client = AzookeyServiceClient::new(server_channel);

        Ok(Self {
            azookey_client,
            runtime: Arc::new(runtime),
        })
    }
}

// implement methods to interact with kkc server
impl IPCService {
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
