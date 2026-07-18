use anyhow::Result;
use shared::proto::{
    azookey_service_client::AzookeyServiceClient, window_service_client::WindowServiceClient,
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::time;
use tonic::transport::Channel;

/// Upper bound for a single IPC round trip. Every request is issued from the
/// host application's UI thread via block_on, so a hung server must fail the
/// request instead of freezing the host application forever.
const RPC_TIMEOUT: Duration = Duration::from_secs(2);

// connect to kkc server
#[derive(Debug, Clone)]
pub struct IPCService {
    // kkc server client
    azookey_client: AzookeyServiceClient<Channel>,
    // candidate window server client
    window_client: WindowServiceClient<Channel>,
    runtime: Arc<tokio::runtime::Runtime>,
}

#[derive(Debug, Clone, Default)]
pub struct Candidates {
    pub texts: Vec<String>,
    pub sub_texts: Vec<String>,
    pub hiragana: String,
    pub corresponding_count: Vec<i32>,
}

impl Candidates {
    /// Returns (text, sub_text, corresponding_count) for the given index.
    /// The engine can return an empty candidate list (and the three vecs are
    /// not guaranteed to have equal lengths), so out-of-bounds access must
    /// degrade to empty values instead of panicking — a panic here unwinds
    /// out of a COM callback and aborts the host application.
    pub fn entry(&self, index: usize) -> (String, String, i32) {
        (
            self.texts.get(index).cloned().unwrap_or_default(),
            self.sub_texts.get(index).cloned().unwrap_or_default(),
            self.corresponding_count.get(index).copied().unwrap_or(0),
        )
    }
}

impl From<shared::proto::ComposingText> for Candidates {
    fn from(composing_text: shared::proto::ComposingText) -> Self {
        Candidates {
            texts: composing_text
                .suggestions
                .iter()
                .map(|s| s.text.clone())
                .collect(),
            sub_texts: composing_text
                .suggestions
                .iter()
                .map(|s| s.subtext.clone())
                .collect(),
            hiragana: composing_text.hiragana,
            corresponding_count: composing_text
                .suggestions
                .iter()
                .map(|s| s.corresponding_count)
                .collect(),
        }
    }
}

impl IPCService {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new()?;

        // Enter the runtime context before building the channels. tonic's
        // connect_with_connector_lazy installs the channel's connection task
        // and needs an ambient Tokio reactor; this runs on the host app's UI
        // thread, which has no runtime otherwise. Without this guard,
        // IPCService::new panics inside Activate with "there is no reactor
        // running", the panic is caught and turned into E_FAIL, and the TIP
        // never activates (the previously active IME's icon stays). The guard
        // is dropped at the end of new(); RPCs later run via runtime.block_on,
        // which enters the context on their own.
        let _guard = runtime.enter();

        // lazy channels: no connection is attempted here. Each RPC connects
        // on demand and tonic re-establishes the connection after transport
        // failures, so the IME recovers automatically when the server or UI
        // process restarts — without re-activating the text service.
        let server_channel = shared::pipe::lazy_pipe_channel(shared::pipe::SERVER_PIPE)?;
        let ui_channel = shared::pipe::lazy_pipe_channel(shared::pipe::UI_PIPE)?;

        let azookey_client = AzookeyServiceClient::new(server_channel);
        let window_client = WindowServiceClient::new(ui_channel);
        tracing::debug!("Created lazy IPC channels: {:?}", azookey_client);

        Ok(Self {
            azookey_client,
            window_client,
            runtime: Arc::new(runtime),
        })
    }

    /// Runs one RPC on the internal runtime with a hard deadline.
    fn exec<T>(
        &self,
        fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T> {
        self.runtime.block_on(async {
            time::timeout(RPC_TIMEOUT, fut)
                .await
                .map_err(|_| anyhow::anyhow!("IPC request timed out after {RPC_TIMEOUT:?}"))?
                .map_err(anyhow::Error::from)
                .map(|response| response.into_inner())
        })
    }
}

// implement methods to interact with kkc server
impl IPCService {
    #[tracing::instrument]
    pub fn append_text(&mut self, text: String) -> anyhow::Result<Candidates> {
        let mut client = self.azookey_client.clone();
        let response = self.exec(async move {
            client
                .append_text(tonic::Request::new(shared::proto::AppendTextRequest {
                    text_to_append: text,
                }))
                .await
        })?;

        response
            .composing_text
            .map(Candidates::from)
            .ok_or_else(|| anyhow::anyhow!("composing_text is None"))
    }

    #[tracing::instrument]
    pub fn remove_text(&mut self) -> anyhow::Result<Candidates> {
        let mut client = self.azookey_client.clone();
        let response = self.exec(async move {
            client
                .remove_text(tonic::Request::new(shared::proto::RemoveTextRequest {}))
                .await
        })?;

        response
            .composing_text
            .map(Candidates::from)
            .ok_or_else(|| anyhow::anyhow!("composing_text is None"))
    }

    #[tracing::instrument]
    pub fn clear_text(&mut self) -> anyhow::Result<()> {
        let mut client = self.azookey_client.clone();
        self.exec(async move {
            client
                .clear_text(tonic::Request::new(shared::proto::ClearTextRequest {}))
                .await
        })?;

        Ok(())
    }

    #[tracing::instrument]
    pub fn shrink_text(&mut self, offset: i32) -> anyhow::Result<Candidates> {
        let mut client = self.azookey_client.clone();
        let response = self.exec(async move {
            client
                .shrink_text(tonic::Request::new(shared::proto::ShrinkTextRequest {
                    offset,
                }))
                .await
        })?;

        response
            .composing_text
            .map(Candidates::from)
            .ok_or_else(|| anyhow::anyhow!("composing_text is None"))
    }

    pub fn set_context(&mut self, context: String) -> anyhow::Result<()> {
        let mut client = self.azookey_client.clone();
        self.exec(async move {
            client
                .set_context(tonic::Request::new(shared::proto::SetContextRequest {
                    context,
                }))
                .await
        })?;

        Ok(())
    }
}

// implement methods to interact with the candidate window server.
// window RPCs are cosmetic: a dead or slow UI process must not break text
// input, so failures are logged and swallowed instead of propagated.
impl IPCService {
    #[tracing::instrument]
    pub fn show_window(&mut self) {
        let mut client = self.window_client.clone();
        let result = self.exec(async move {
            client
                .show_window(tonic::Request::new(shared::proto::EmptyResponse {}))
                .await
        });
        if let Err(e) = result {
            tracing::warn!("show_window failed: {e}");
        }
    }

    #[tracing::instrument]
    pub fn hide_window(&mut self) {
        let mut client = self.window_client.clone();
        let result = self.exec(async move {
            client
                .hide_window(tonic::Request::new(shared::proto::EmptyResponse {}))
                .await
        });
        if let Err(e) = result {
            tracing::warn!("hide_window failed: {e}");
        }
    }

    #[tracing::instrument]
    pub fn set_window_position(&mut self, top: i32, left: i32, bottom: i32, right: i32) {
        let mut client = self.window_client.clone();
        let result = self.exec(async move {
            client
                .set_window_position(tonic::Request::new(shared::proto::SetPositionRequest {
                    position: Some(shared::proto::WindowPosition {
                        top,
                        left,
                        bottom,
                        right,
                    }),
                }))
                .await
        });
        if let Err(e) = result {
            tracing::warn!("set_window_position failed: {e}");
        }
    }

    #[tracing::instrument]
    pub fn set_candidates(&mut self, candidates: Vec<String>) {
        let mut client = self.window_client.clone();
        let result = self.exec(async move {
            client
                .set_candidate(tonic::Request::new(shared::proto::SetCandidateRequest {
                    candidates,
                }))
                .await
        });
        if let Err(e) = result {
            tracing::warn!("set_candidates failed: {e}");
        }
    }

    #[tracing::instrument]
    pub fn set_selection(&mut self, index: i32) {
        let mut client = self.window_client.clone();
        let result = self.exec(async move {
            client
                .set_selection(tonic::Request::new(shared::proto::SetSelectionRequest {
                    index,
                }))
                .await
        });
        if let Err(e) = result {
            tracing::warn!("set_selection failed: {e}");
        }
    }

    #[tracing::instrument]
    pub fn set_input_mode(&mut self, mode: &str) {
        let mut client = self.window_client.clone();
        let mode = mode.to_string();
        let result = self.exec(async move {
            client
                .set_input_mode(tonic::Request::new(shared::proto::SetInputModeRequest {
                    mode,
                }))
                .await
        });
        if let Err(e) = result {
            tracing::warn!("set_input_mode failed: {e}");
        }
    }
}
