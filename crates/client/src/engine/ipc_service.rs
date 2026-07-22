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

/// Marker error meaning the conversion server could not be reached: it
/// crashed, is restarting (the launcher supervises and relaunches it), or
/// hung past [`RPC_TIMEOUT`]. Distinct from a server-side logic error so the
/// composition layer can tell "the server lost my state" (reset the client
/// composition) from "the server rejected this request" (surface as-is). The
/// engine RPCs tag their transport/timeout failures with this; a normal RPC
/// on a live server never produces it, so reacting to it cannot disturb the
/// happy typing path.
#[derive(Debug)]
pub struct ServerUnavailable;

impl std::fmt::Display for ServerUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "azookey server is unavailable")
    }
}

impl std::error::Error for ServerUnavailable {}

// connect to kkc server
#[derive(Debug, Clone)]
pub struct IPCService {
    // kkc server client
    azookey_client: AzookeyServiceClient<Channel>,
    // candidate window server client
    window_client: WindowServiceClient<Channel>,
    runtime: Arc<tokio::runtime::Runtime>,
    /// Test seam: when set, every RPC method delegates to this recorder
    /// instead of the wire. The engine RPCs need a live server, so
    /// handle_action's golden tests cannot run against the real clients.
    /// Arc/Mutex because the service is shared through the global IMEState
    /// (Send) and cloned per handle_action call.
    #[cfg(test)]
    fake: Option<Arc<std::sync::Mutex<FakeIpc>>>,
}

#[derive(Debug, Clone, Default)]
pub struct Candidates {
    pub texts: Vec<String>,
    pub sub_texts: Vec<String>,
    pub hiragana: String,
    /// romaji keystrokes each candidate covers, for `raw_input`
    pub corresponding_count: Vec<i32>,
    /// kana of the reading each candidate covers, for `ShrinkText`
    pub surface_count: Vec<i32>,
}

impl Candidates {
    /// Returns (text, sub_text, corresponding_count, surface_count) for the
    /// given index. The engine can return an empty candidate list (and the
    /// vecs are not guaranteed to have equal lengths), so out-of-bounds
    /// access must degrade to empty values instead of panicking — a panic
    /// here unwinds out of a COM callback and aborts the host application.
    pub fn entry(&self, index: usize) -> (String, String, i32, i32) {
        (
            self.texts.get(index).cloned().unwrap_or_default(),
            self.sub_texts.get(index).cloned().unwrap_or_default(),
            self.corresponding_count.get(index).copied().unwrap_or(0),
            self.surface_count.get(index).copied().unwrap_or(0),
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
            surface_count: composing_text
                .suggestions
                .iter()
                .map(|s| s.surface_count)
                .collect(),
        }
    }
}

/// Unwraps the `composing_text` an engine RPC answers with into `Candidates`.
/// A `None` is a protocol error — the caller always sent text to convert, so
/// the server owed a reading back — and becomes an ordinary error rather than
/// a silent empty list.
fn candidates_or_missing(composing_text: Option<shared::proto::ComposingText>) -> Result<Candidates> {
    composing_text
        .map(Candidates::from)
        .ok_or_else(|| anyhow::anyhow!("composing_text is None"))
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
        let server_channel = shared::pipe::lazy_pipe_channel(shared::pipe::server_pipe())?;
        let ui_channel = shared::pipe::lazy_pipe_channel(shared::pipe::ui_pipe())?;

        let azookey_client = AzookeyServiceClient::new(server_channel);
        let window_client = WindowServiceClient::new(ui_channel);
        tracing::debug!("Created lazy IPC channels: {:?}", azookey_client);

        Ok(Self {
            azookey_client,
            window_client,
            runtime: Arc::new(runtime),
            #[cfg(test)]
            fake: None,
        })
    }

    /// Builds a service whose RPCs are answered by a shared [`FakeIpc`]
    /// recorder instead of the wire. The real lazy channels are still
    /// constructed (they never connect) so the production fields stay
    /// identical.
    #[cfg(test)]
    pub fn new_fake() -> Result<(Self, Arc<std::sync::Mutex<FakeIpc>>)> {
        let fake = Arc::new(std::sync::Mutex::new(FakeIpc::default()));
        let mut service = Self::new()?;
        service.fake = Some(fake.clone());
        Ok((service, fake))
    }

    /// Runs one call against the fake, if installed. `Some(result)` short-
    /// circuits the caller; `None` means no fake — go to the wire.
    #[cfg(test)]
    fn fake_call<T>(&self, call: impl FnOnce(&mut FakeIpc) -> T) -> Option<T> {
        self.fake.as_ref().map(|fake| {
            let mut fake = fake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            call(&mut fake)
        })
    }

    /// Runs one RPC on the internal runtime with a hard deadline. A timeout or
    /// a transport-level failure (`Unavailable` — the pipe is gone because the
    /// server crashed/restarted) is tagged [`ServerUnavailable`] so callers can
    /// recover the composition; a server-side status (e.g. `InvalidArgument`)
    /// is surfaced unchanged.
    fn exec<T>(
        &self,
        fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T> {
        self.runtime.block_on(async {
            match time::timeout(RPC_TIMEOUT, fut).await {
                Err(_) => Err(anyhow::Error::new(ServerUnavailable)
                    .context(format!("IPC request timed out after {RPC_TIMEOUT:?}"))),
                Ok(Err(status)) if status.code() == tonic::Code::Unavailable => {
                    Err(anyhow::Error::new(ServerUnavailable)
                        .context(format!("IPC transport error: {status}")))
                }
                Ok(Err(status)) => Err(anyhow::Error::from(status)),
                Ok(Ok(response)) => Ok(response.into_inner()),
            }
        })
    }

    /// The wire tail shared by every candidate-window RPC: clone the window
    /// client, run one call through `exec`, and swallow any failure with a
    /// warning. Window RPCs are cosmetic — a dead or slow UI process must not
    /// break text input — so unlike the engine RPCs they never propagate. The
    /// `#[cfg(test)]` recording stays in each method because the `IpcCall`
    /// variants differ; this is only the production path.
    fn window_rpc<T, Fut>(&self, name: &str, call: impl FnOnce(WindowServiceClient<Channel>) -> Fut)
    where
        Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        let client = self.window_client.clone();
        if let Err(e) = self.exec(call(client)) {
            tracing::warn!("{name} failed: {e}");
        }
    }
}

/// Recording double behind the `fake` seam: keeps every call in order and
/// answers the engine RPCs from a script, so handle_action's golden tests
/// can assert both the state written back AND the IPC traffic an action
/// produced — without a live server or UI process.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeIpc {
    /// every RPC in call order
    pub calls: Vec<IpcCall>,
    /// what append_text/remove_text/shrink_text answer
    pub scripted_candidates: Candidates,
    /// when true, the engine RPCs fail like a dead server (window RPCs
    /// stay silent, mirroring the real cosmetic/advisory split)
    pub engine_fails: bool,
    /// when true, the engine RPCs fail with [`ServerUnavailable`], simulating
    /// a crashed/restarting server (transport gone) rather than a generic
    /// error — the trigger for the composition-reset recovery path
    pub engine_unavailable: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub enum IpcCall {
    AppendText(String),
    RemoveText,
    ClearText,
    ShrinkText(i32),
    SetContext(String),
    ShowWindow,
    HideWindow,
    SetWindowPosition,
    SetCandidates(Vec<String>),
    SetSelection(i32),
    SetInputMode(String),
}

#[cfg(test)]
impl FakeIpc {
    fn engine_answer(&mut self, call: IpcCall) -> anyhow::Result<Candidates> {
        self.calls.push(call);
        if self.engine_unavailable {
            return Err(anyhow::Error::new(ServerUnavailable).context("fake server unavailable"));
        }
        if self.engine_fails {
            anyhow::bail!("fake engine is down");
        }
        Ok(self.scripted_candidates.clone())
    }
}

// implement methods to interact with kkc server
impl IPCService {
    // skip(self) on every RPC below: IPCService's Debug is the two tonic
    // channels plus the tokio runtime, ~3 KB of boilerplate per span that
    // says nothing about the call. The arguments are the interesting part.
    #[tracing::instrument(skip(self))]
    pub fn append_text(&mut self, text: String) -> anyhow::Result<Candidates> {
        #[cfg(test)]
        if let Some(result) =
            self.fake_call(|fake| fake.engine_answer(IpcCall::AppendText(text.clone())))
        {
            return result;
        }

        let mut client = self.azookey_client.clone();
        let response = self.exec(async move {
            client
                .append_text(tonic::Request::new(shared::proto::AppendTextRequest {
                    text_to_append: text,
                }))
                .await
        })?;

        candidates_or_missing(response.composing_text)
    }

    #[tracing::instrument(skip(self))]
    pub fn remove_text(&mut self) -> anyhow::Result<Candidates> {
        #[cfg(test)]
        if let Some(result) = self.fake_call(|fake| fake.engine_answer(IpcCall::RemoveText)) {
            return result;
        }

        let mut client = self.azookey_client.clone();
        let response = self.exec(async move {
            client
                .remove_text(tonic::Request::new(shared::proto::RemoveTextRequest {}))
                .await
        })?;

        candidates_or_missing(response.composing_text)
    }

    #[tracing::instrument(skip(self))]
    pub fn clear_text(&mut self) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(result) = self.fake_call(|fake| {
            fake.calls.push(IpcCall::ClearText);
            if fake.engine_fails {
                anyhow::bail!("fake engine is down");
            }
            Ok(())
        }) {
            return result;
        }

        let mut client = self.azookey_client.clone();
        self.exec(async move {
            client
                .clear_text(tonic::Request::new(shared::proto::ClearTextRequest {}))
                .await
        })?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    /// `surface_offset` is a count of kana in the reading, not of keystrokes:
    /// a candidate can end inside a romaji cluster and only the kana
    /// boundary can say where.
    pub fn shrink_text(&mut self, surface_offset: i32) -> anyhow::Result<Candidates> {
        #[cfg(test)]
        if let Some(result) =
            self.fake_call(|fake| fake.engine_answer(IpcCall::ShrinkText(surface_offset)))
        {
            return result;
        }

        let mut client = self.azookey_client.clone();
        let response = self.exec(async move {
            client
                .shrink_text(tonic::Request::new(shared::proto::ShrinkTextRequest {
                    surface_offset,
                }))
                .await
        })?;

        candidates_or_missing(response.composing_text)
    }

    pub fn set_context(&mut self, context: String) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(result) = self.fake_call(|fake| {
            fake.calls.push(IpcCall::SetContext(context.clone()));
            Ok(())
        }) {
            return result;
        }

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
    #[tracing::instrument(skip(self))]
    pub fn show_window(&mut self) {
        #[cfg(test)]
        if self
            .fake_call(|fake| fake.calls.push(IpcCall::ShowWindow))
            .is_some()
        {
            return;
        }

        self.window_rpc("show_window", |mut client| async move {
            client
                .show_window(tonic::Request::new(shared::proto::EmptyResponse {}))
                .await
        });
    }

    #[tracing::instrument(skip(self))]
    pub fn hide_window(&mut self) {
        #[cfg(test)]
        if self
            .fake_call(|fake| fake.calls.push(IpcCall::HideWindow))
            .is_some()
        {
            return;
        }

        self.window_rpc("hide_window", |mut client| async move {
            client
                .hide_window(tonic::Request::new(shared::proto::EmptyResponse {}))
                .await
        });
    }

    #[tracing::instrument(skip(self))]
    pub fn set_window_position(&mut self, top: i32, left: i32, bottom: i32, right: i32) {
        #[cfg(test)]
        if self
            .fake_call(|fake| fake.calls.push(IpcCall::SetWindowPosition))
            .is_some()
        {
            return;
        }

        self.window_rpc("set_window_position", |mut client| async move {
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
    }

    #[tracing::instrument(skip(self))]
    pub fn set_candidates(&mut self, candidates: Vec<String>) {
        #[cfg(test)]
        if self
            .fake_call(|fake| fake.calls.push(IpcCall::SetCandidates(candidates.clone())))
            .is_some()
        {
            return;
        }

        self.window_rpc("set_candidates", |mut client| async move {
            client
                .set_candidate(tonic::Request::new(shared::proto::SetCandidateRequest {
                    candidates,
                }))
                .await
        });
    }

    #[tracing::instrument(skip(self))]
    pub fn set_selection(&mut self, index: i32) {
        #[cfg(test)]
        if self
            .fake_call(|fake| fake.calls.push(IpcCall::SetSelection(index)))
            .is_some()
        {
            return;
        }

        self.window_rpc("set_selection", |mut client| async move {
            client
                .set_selection(tonic::Request::new(shared::proto::SetSelectionRequest {
                    index,
                }))
                .await
        });
    }

    #[tracing::instrument(skip(self))]
    pub fn set_input_mode(&mut self, mode: &str) {
        #[cfg(test)]
        if self
            .fake_call(|fake| fake.calls.push(IpcCall::SetInputMode(mode.to_string())))
            .is_some()
        {
            return;
        }

        let mode = mode.to_string();
        self.window_rpc("set_input_mode", |mut client| async move {
            client
                .set_input_mode(tonic::Request::new(shared::proto::SetInputModeRequest {
                    mode,
                }))
                .await
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Regression guard for the reactor-context panic fixed in 70d367c.
    ///
    /// `IPCService::new` builds tonic channels via
    /// `connect_with_connector_lazy`, which needs an ambient Tokio reactor.
    /// In production it runs on the host application's UI thread, which has
    /// no runtime — exactly like this `#[test]`, which the test harness runs
    /// on a plain thread with no ambient runtime. The `runtime.enter()` guard
    /// inside `new` supplies the context. If that guard is ever removed, this
    /// call panics with "there is no reactor running, must be called from the
    /// context of a Tokio 1.x runtime", the panic is turned into E_FAIL in
    /// Activate, and the TIP never activates (the previous IME's icon stays).
    ///
    /// Note: this must NOT be a `#[tokio::test]` — that would install an
    /// ambient runtime and mask the very regression it guards against.
    #[test]
    fn new_does_not_need_an_ambient_runtime() {
        let service = IPCService::new();
        assert!(
            service.is_ok(),
            "IPCService::new must succeed without an ambient Tokio runtime \
             (channels are lazy, so no connection is attempted): {:?}",
            service.err()
        );
    }

    /// Activate runs on every IME switch, so `new` is called repeatedly over a
    /// session. Each call stands up its own runtime and lazy channels; none of
    /// them may depend on a runtime left over from a previous call.
    #[test]
    fn new_can_be_called_repeatedly() {
        for i in 0..3 {
            assert!(
                IPCService::new().is_ok(),
                "IPCService::new failed on call {i}"
            );
        }
    }
}
