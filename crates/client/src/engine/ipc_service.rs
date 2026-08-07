use anyhow::Result;
use shared::proto::{
    azookey_service_client::AzookeyServiceClient, window_service_client::WindowServiceClient,
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::time;
use tonic::transport::Channel;

/// Deadlines, per kind of call. Every request is issued from the host
/// application's UI thread via block_on, so the deadline is how long a hung
/// process may freeze the host — one budget for all of them charged the
/// slowest call's worst case to the fastest ones.
///
/// Upper bound for a call that actually converts (append/remove/shrink). The
/// engine may run neural inference here, so this stays the generous two
/// seconds it has always been: shortening it would fail honest conversions on
/// slow hardware, which is a worse bug than the one being fixed.
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound for an engine call that only moves state around
/// (clear/set-context). No dictionary or model is touched, so anything past a
/// few milliseconds already means the server is not answering — waiting the
/// full conversion budget would just hold the host thread longer.
const HOUSEKEEPING_TIMEOUT: Duration = Duration::from_millis(500);

/// Upper bound for a candidate-window call. These are cosmetic and several
/// fire per keystroke, so a hung `ui.exe` must cost the typist as little as
/// possible; the text still goes in without them.
const WINDOW_TIMEOUT: Duration = Duration::from_millis(300);

/// How many times an IDEMPOTENT engine RPC may be issued before giving up.
///
/// Retrying is only sound when running the call twice is indistinguishable
/// from running it once — `ClearText` and `SetContext` overwrite state, so
/// they qualify; `AppendText`/`RemoveText`/`ShrinkText`/`MoveCursor` all edit
/// the reading relative to itself and must never be replayed on their own (a
/// resend that the first attempt actually delivered would type the keystroke
/// twice). The whole-composition rebuild in `composition.rs` is how those
/// recover instead: it re-establishes the reading from scratch, which is
/// idempotent even though its parts are not.
///
/// Two attempts rather than more because the failure this covers is a
/// one-shot: tonic's lazy channel notices the old pipe is gone on the call
/// that fails and dials the restarted server on the next one.
const IDEMPOTENT_ATTEMPTS: u32 = 2;

/// Marker error meaning the conversion server could not be reached: it
/// crashed, is restarting (the launcher supervises and relaunches it), or
/// hung past the deadline for its kind of call. Distinct from a server-side
/// logic error so the
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

/// Re-issues an IDEMPOTENT RPC that failed because the server was
/// unreachable, up to [`IDEMPOTENT_ATTEMPTS`] times in total. See that
/// constant for which RPCs may come through here and why the rest may not.
/// Only [`ServerUnavailable`] is retried: a server-side rejection travelled
/// over a working pipe and would be rejected again.
///
/// Wraps the whole call, not just the wire, so the attempt policy is part of
/// the RPC's contract rather than an implementation detail of the transport.
fn retry_idempotent<T>(mut call: impl FnMut() -> Result<T>) -> Result<T> {
    // Counted attempts rather than "loop N-1 times, then call once more":
    // the old shape spent the same budget but made the count off by one to
    // read, and the number of attempts is the whole contract here.
    let mut attempt = 1;
    loop {
        let result = call();
        match &result {
            Err(error) if is_server_unavailable(error) && attempt < IDEMPOTENT_ATTEMPTS => {
                tracing::warn!(
                    "idempotent IPC attempt {attempt}/{IDEMPOTENT_ATTEMPTS} failed, \
                     retrying: {error:#}"
                );
            }
            _ => return result,
        }
        attempt += 1;
    }
}

/// True when `error` (or any of its causes) carries the [`ServerUnavailable`]
/// tag, i.e. an engine RPC failed because the conversion server could not be
/// reached rather than because it rejected the request. `exec` attaches the
/// tag as the root cause under a `context`, so the whole chain is walked.
pub fn is_server_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<ServerUnavailable>())
}

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

/// One update to what the candidate window shows.
///
/// Every field is optional and `None` means "leave it as it is" — which is
/// what lets the list, the highlight and the caret position travel together
/// or separately over the same RPC. The distinction matters most for the
/// list: moving the highlight must NOT resend the candidates, and an empty
/// list (`Some(vec![])`, the blank a composition ends with) is not the same
/// thing as not touching the list at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CandidateView {
    pub candidates: Option<Vec<String>>,
    pub selection: Option<i32>,
    /// The caret rect, as (top, left, bottom, right).
    pub position: Option<(i32, i32, i32, i32)>,
}

impl CandidateView {
    /// A fresh list with the highlight that goes with it — the keystroke
    /// path, and the reason this type exists.
    pub fn list_and_selection(candidates: Vec<String>, selection: i32) -> Self {
        Self {
            candidates: Some(candidates),
            selection: Some(selection),
            position: None,
        }
    }

    /// The highlight alone (the arrow keys). The list is deliberately left
    /// out: resending it on every arrow press is the traffic this replaced.
    pub fn selection(selection: i32) -> Self {
        Self {
            selection: Some(selection),
            ..Self::default()
        }
    }

    pub fn list(candidates: Vec<String>) -> Self {
        Self {
            candidates: Some(candidates),
            ..Self::default()
        }
    }

    pub fn at(top: i32, left: i32, bottom: i32, right: i32) -> Self {
        Self {
            position: Some((top, left, bottom, right)),
            ..Self::default()
        }
    }

    fn into_request(self) -> shared::proto::UpdateCandidateViewRequest {
        shared::proto::UpdateCandidateViewRequest {
            candidates: self
                .candidates
                .map(|texts| shared::proto::CandidateList { texts }),
            selection: self.selection,
            position: self.position.map(|(top, left, bottom, right)| {
                shared::proto::WindowPosition {
                    top,
                    left,
                    bottom,
                    right,
                }
            }),
        }
    }
}

/// Unwraps the `composing_text` an engine RPC answers with into `Candidates`.
/// A `None` is a protocol error — the caller always sent text to convert, so
/// the server owed a reading back — and becomes an ordinary error rather than
/// a silent empty list.
fn candidates_or_missing(
    composing_text: Option<shared::proto::ComposingText>,
) -> Result<Candidates> {
    composing_text
        .map(Candidates::from)
        .ok_or_else(|| anyhow::anyhow!("composing_text is None"))
}

impl IPCService {
    pub fn new() -> Result<Self> {
        // The runtime, and the channels built inside its context — see
        // shared::pipe::blocking_channels for why the context matters here.
        let (runtime, (server_channel, ui_channel)) = shared::pipe::blocking_channels(|| {
            Ok::<_, anyhow::Error>((
                shared::pipe::lazy_pipe_channel(shared::pipe::server_pipe())?,
                shared::pipe::lazy_pipe_channel(shared::pipe::ui_pipe())?,
            ))
        })?;

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
    /// a transport-level failure (the pipe is gone because the server
    /// crashed/restarted — see [`shared::pipe::is_transport_failure`] for why
    /// that is not just `Unavailable`) is tagged [`ServerUnavailable`] so
    /// callers can recover the composition; a server-side status (e.g.
    /// `InvalidArgument`) is surfaced unchanged.
    ///
    /// On timeout the future is dropped, which cancels the tonic request: a
    /// reply that arrives afterwards has nowhere to go and is discarded. The
    /// SERVER may still have applied the call, so a caller that recovers must
    /// resynchronize rather than assume the request never happened.
    fn exec<T>(
        &self,
        deadline: Duration,
        fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T> {
        self.runtime.block_on(async {
            match time::timeout(deadline, fut).await {
                Err(_) => Err(anyhow::Error::new(ServerUnavailable)
                    .context(format!("IPC request timed out after {deadline:?}"))),
                Ok(Err(status)) if shared::pipe::is_transport_failure(&status) => {
                    Err(anyhow::Error::new(ServerUnavailable)
                        .context(format!("IPC transport error: {status}")))
                }
                Ok(Err(status)) => Err(anyhow::Error::from(status)),
                Ok(Ok(response)) => Ok(response.into_inner()),
            }
        })
    }

    /// `exec` for the ENGINE RPCs, which additionally records what the
    /// outcome says about the server's reachability so the language bar can
    /// report it (issue #79). Not used by the window RPCs: those talk to
    /// `ui.exe`, and a dead candidate window is a different fault from a dead
    /// conversion engine.
    fn engine_exec<T>(
        &self,
        deadline: Duration,
        fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T> {
        let result = self.exec(deadline, fut);
        super::engine_health::record(&result);
        result
    }

    /// The wire tail shared by every candidate-window RPC: clone the window
    /// client, run one call through `exec`, and swallow any failure with a
    /// warning. Window RPCs are cosmetic — a dead or slow UI process must not
    /// break text input — so unlike the engine RPCs they never propagate.
    fn window_rpc<T, Fut>(&self, name: &str, call: impl FnOnce(WindowServiceClient<Channel>) -> Fut)
    where
        Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        let client = self.window_client.clone();
        if let Err(e) = self.exec(WINDOW_TIMEOUT, call(client)) {
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
    /// how many further engine RPCs fail with [`ServerUnavailable`] before the
    /// server answers again. Models the fault the non-destructive rebuild
    /// exists for — a stall or a restart the server comes back from — as
    /// opposed to `engine_unavailable`, which never recovers.
    pub engine_unavailable_for: u32,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub enum IpcCall {
    AppendText(String),
    RemoveText(i32),
    /// The index carried is the candidate the user confirmed, or `None`
    /// when the composition was discarded rather than confirmed.
    ClearText(Option<i32>),
    ShrinkText(i32, Option<i32>),
    SetContext(String),
    ShowWindow,
    HideWindow,
    UpdateCandidateView(CandidateView),
    SetInputMode(String),
}

#[cfg(test)]
impl FakeIpc {
    /// The failure switch every engine RPC of the fake goes through, so a
    /// scripted outage covers `clear_text` too — the rebuild path starts with
    /// it, and a fake that answered it while the rest of the engine was down
    /// would never exercise a failing rebuild.
    fn engine_outage(&mut self) -> anyhow::Result<()> {
        if self.engine_unavailable_for > 0 {
            self.engine_unavailable_for -= 1;
            return Err(anyhow::Error::new(ServerUnavailable).context("fake server is stalled"));
        }
        if self.engine_unavailable {
            return Err(anyhow::Error::new(ServerUnavailable).context("fake server unavailable"));
        }
        if self.engine_fails {
            anyhow::bail!("fake engine is down");
        }
        Ok(())
    }

    fn engine_answer(&mut self, call: IpcCall) -> anyhow::Result<Candidates> {
        self.calls.push(call);
        self.engine_outage()?;
        Ok(self.scripted_candidates.clone())
    }
}

// implement methods to interact with kkc server
//
// Every RPC takes `&self`: the clients are cloned per call (that is how tonic
// is meant to be used) and nothing about the service itself changes, so a
// `&mut` would only have forced the callers — the eight act_* arms and every
// advisory path — to thread a mutable borrow they never needed.
impl IPCService {
    // skip(self) on every RPC below: IPCService's Debug is the two tonic
    // channels plus the tokio runtime, ~3 KB of boilerplate per span that
    // says nothing about the call. The arguments are the interesting part.
    #[tracing::instrument(skip(self))]
    pub fn append_text(&self, text: String) -> anyhow::Result<Candidates> {
        #[cfg(test)]
        if let Some(result) =
            self.fake_call(|fake| fake.engine_answer(IpcCall::AppendText(text.clone())))
        {
            return result;
        }

        let mut client = self.azookey_client.clone();
        let response = self.engine_exec(CONVERSION_TIMEOUT, async move {
            client
                .append_text(tonic::Request::new(shared::proto::AppendTextRequest {
                    text_to_append: text,
                }))
                .await
        })?;

        candidates_or_missing(response.composing_text)
    }

    /// Deletes `count` kana from the end of the reading in one call. The
    /// server converts once for the batch, which is what makes a held
    /// Backspace cheap; see `RemoveTextRequest.count` in service.proto.
    #[tracing::instrument(skip(self))]
    pub fn remove_text(&self, count: u32) -> anyhow::Result<Candidates> {
        // Saturating rather than wrapping: the wire field is int32 and a
        // count this large is already nonsense, but a negative one would make
        // the server clamp it back to 1 and silently delete the wrong amount.
        let count = i32::try_from(count).unwrap_or(i32::MAX);

        #[cfg(test)]
        if let Some(result) = self.fake_call(|fake| fake.engine_answer(IpcCall::RemoveText(count)))
        {
            return result;
        }

        let mut client = self.azookey_client.clone();
        let response = self.engine_exec(CONVERSION_TIMEOUT, async move {
            client
                .remove_text(tonic::Request::new(shared::proto::RemoveTextRequest {
                    count,
                }))
                .await
        })?;

        candidates_or_missing(response.composing_text)
    }

    /// Ends the composition. `candidate_index` names the candidate the user
    /// confirmed — the row of the list this client was last shown — so the
    /// engine can learn from it, and is `None` for every way a composition
    /// ends WITHOUT being confirmed: Escape, focus loss, a host that
    /// terminated it, a rebuild after an outage.
    ///
    /// Idempotent: clearing an already-cleared reading lands on the same
    /// state, so it may be retried — and it is the first half of the
    /// whole-composition rebuild, which is worth the extra attempt. That
    /// stays true with an index attached: the engine drops its record of
    /// what this session was offered as part of handling the first call, so
    /// a resend of the same confirmation finds nothing to learn from and
    /// cannot double-count it.
    #[tracing::instrument(skip(self))]
    pub fn clear_text(&self, candidate_index: Option<i32>) -> anyhow::Result<()> {
        retry_idempotent(|| self.clear_text_once(candidate_index))
    }

    fn clear_text_once(&self, candidate_index: Option<i32>) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(result) = self.fake_call(|fake| {
            fake.calls.push(IpcCall::ClearText(candidate_index));
            fake.engine_outage()
        }) {
            return result;
        }

        let mut client = self.azookey_client.clone();
        self.engine_exec(HOUSEKEEPING_TIMEOUT, async move {
            client
                .clear_text(tonic::Request::new(shared::proto::ClearTextRequest {
                    candidate_index,
                }))
                .await
        })?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    /// `surface_offset` is a count of kana in the reading, not of keystrokes:
    /// a candidate can end inside a romaji cluster and only the kana
    /// boundary can say where.
    ///
    /// `candidate_index`: see [`IPCService::clear_text`]. A clause commit is
    /// a confirmation like any other, so it carries one.
    pub fn shrink_text(
        &self,
        surface_offset: i32,
        candidate_index: Option<i32>,
    ) -> anyhow::Result<Candidates> {
        #[cfg(test)]
        if let Some(result) = self.fake_call(|fake| {
            fake.engine_answer(IpcCall::ShrinkText(surface_offset, candidate_index))
        }) {
            return result;
        }

        let mut client = self.azookey_client.clone();
        let response = self.engine_exec(CONVERSION_TIMEOUT, async move {
            client
                .shrink_text(tonic::Request::new(shared::proto::ShrinkTextRequest {
                    surface_offset,
                    candidate_index,
                }))
                .await
        })?;

        candidates_or_missing(response.composing_text)
    }

    /// Idempotent: the context is overwritten wholesale, so a resend that the
    /// first attempt already delivered changes nothing.
    pub fn set_context(&self, context: String) -> anyhow::Result<()> {
        retry_idempotent(|| self.set_context_once(context.clone()))
    }

    fn set_context_once(&self, context: String) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(result) = self.fake_call(|fake| {
            fake.calls.push(IpcCall::SetContext(context.clone()));
            Ok(())
        }) {
            return result;
        }

        let mut client = self.azookey_client.clone();
        self.engine_exec(HOUSEKEEPING_TIMEOUT, async move {
            client
                .set_context(tonic::Request::new(shared::proto::SetContextRequest {
                    context,
                }))
                .await
        })?;

        Ok(())
    }
}

/// Generates the candidate-window RPCs, which are structurally identical:
/// record the call against the test fake if one is installed and stop there,
/// otherwise send one request through [`IPCService::window_rpc`]. Only the
/// arguments, the recorded [`IpcCall`] and the request differ; written out by
/// hand, the six methods were ~115 lines of which ~90 were the same six lines
/// repeated.
///
/// The `record:` expression runs BEFORE the request is built, so it may clone
/// out of an argument the request then moves.
macro_rules! window_rpcs {
    ($(
        $(#[$meta:meta])*
        fn $name:ident($($arg:ident: $ty:ty),* $(,)?) {
            record: $record:expr,
            $rpc:ident: $request:expr $(,)?
        }
    )*) => {
        // window RPCs are cosmetic: a dead or slow UI process must not break
        // text input, so failures are logged and swallowed, never propagated.
        impl IPCService {$(
            $(#[$meta])*
            #[tracing::instrument(skip(self))]
            pub fn $name(&self, $($arg: $ty),*) {
                #[cfg(test)]
                if self.fake_call(|fake| fake.calls.push($record)).is_some() {
                    return;
                }

                self.window_rpc(stringify!($name), |mut client| async move {
                    client.$rpc(tonic::Request::new($request)).await
                });
            }
        )*}
    };
}

window_rpcs! {
    fn show_window() {
        record: IpcCall::ShowWindow,
        show_window: shared::proto::EmptyResponse {},
    }

    fn hide_window() {
        record: IpcCall::HideWindow,
        hide_window: shared::proto::EmptyResponse {},
    }

    /// The one candidate-window content RPC. Everything the window draws goes
    /// through it, so a keystroke costs one round trip instead of the two the
    /// separate list and selection calls used to cost — each of them charged
    /// to the host application's UI thread, which blocks on them.
    fn update_candidate_view(view: CandidateView) {
        record: IpcCall::UpdateCandidateView(view.clone()),
        update_candidate_view: view.into_request(),
    }

    /// `String`, not `&str`: the request is built inside an `async move`
    /// block, which cannot borrow the caller's stack.
    fn set_input_mode(mode: String) {
        record: IpcCall::SetInputMode(mode.clone()),
        set_input_mode: shared::proto::SetInputModeRequest { mode },
    }
}

/// The three thin wrappers over [`IPCService::update_candidate_view`] for
/// callers that only ever change one part of the view.
///
/// `set_window_position` in particular: the caret position has a cadence of
/// its own (throttled, off the keystroke path — see `UpdatePosState`), and
/// keeping the name means the edit session that reports it did not have to
/// learn about the combined RPC.
impl IPCService {
    pub fn set_window_position(&self, top: i32, left: i32, bottom: i32, right: i32) {
        self.update_candidate_view(CandidateView::at(top, left, bottom, right));
    }

    pub fn set_candidates(&self, candidates: Vec<String>) {
        self.update_candidate_view(CandidateView::list(candidates));
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

    /// A server that accepts the call and then never answers is the fault the
    /// deadline exists for: `exec` runs on the host application's UI thread,
    /// so without one the host would hang for as long as the engine does.
    /// A future that never completes stands in for the hung server; the
    /// deadline must turn it into `ServerUnavailable` — the tag the
    /// composition layer recovers from — rather than waiting.
    #[test]
    fn a_call_that_never_answers_fails_as_unavailable_at_the_deadline() {
        let service = IPCService::new().unwrap();
        let deadline = Duration::from_millis(50);

        let started = std::time::Instant::now();
        let result: Result<()> = service.exec(deadline, std::future::pending());
        let waited = started.elapsed();

        let error = result.expect_err("a hung server must fail the call, not hang the host");
        assert!(
            is_server_unavailable(&error),
            "a timeout must be tagged ServerUnavailable so the composition can \
             be recovered: {error:#}"
        );
        assert!(
            waited < Duration::from_secs(1),
            "the host thread was held for {waited:?}, far past the {deadline:?} deadline"
        );
    }

    /// The deadlines are ordered by what the call actually does, and the
    /// ordering is the point: a cosmetic candidate-window call must not be
    /// allowed to hold the typist for as long as a neural conversion, and
    /// clearing a reading does no conversion work at all. Pinned here because
    /// the values are otherwise only visible at their call sites.
    #[test]
    fn the_deadlines_are_ordered_by_how_much_work_the_call_does() {
        assert!(
            WINDOW_TIMEOUT < HOUSEKEEPING_TIMEOUT,
            "a cosmetic window call must give up before an engine call does"
        );
        assert!(
            HOUSEKEEPING_TIMEOUT < CONVERSION_TIMEOUT,
            "state-shuffling must give up before conversion, which may run \
             neural inference"
        );
        assert_eq!(
            CONVERSION_TIMEOUT,
            Duration::from_secs(2),
            "the conversion budget must not shrink: honest conversions on slow \
             hardware take seconds, and failing them would be a worse bug than \
             the stall this deadline guards against"
        );
    }

    /// An idempotent RPC gets a second attempt because tonic's lazy channel
    /// only notices a dead pipe on the call that fails — the retry is what
    /// dials the restarted server. It must be exactly one extra attempt: the
    /// host thread is charged the deadline for each.
    #[test]
    fn an_idempotent_rpc_is_retried_exactly_once() {
        let (service, fake) = IPCService::new_fake().unwrap();
        // the whole outage lasts one call, so the retry is what succeeds
        fake.lock().unwrap().engine_unavailable_for = 1;
        service
            .clear_text(None)
            .expect("the retry must carry the call through a one-call outage");

        // ...and an outage that outlives both attempts still surfaces
        let (service, fake) = IPCService::new_fake().unwrap();
        fake.lock().unwrap().engine_unavailable_for = IDEMPOTENT_ATTEMPTS;
        let error = service
            .clear_text(None)
            .expect_err("an outage past the attempt budget must surface");
        assert!(is_server_unavailable(&error), "{error:#}");
        assert_eq!(
            fake.lock().unwrap().engine_unavailable_for,
            0,
            "both attempts must have been spent, and no more than that"
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
