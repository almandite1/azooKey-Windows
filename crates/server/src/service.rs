//! The gRPC service implementation: request/response plumbing only.
//! Engine access goes through the safe wrappers in wrappers.rs.

use tonic::{Request, Response, Status};

use shared::proto::azookey_service_server::AzookeyService;
use shared::proto::{
    AppendTextRequest, AppendTextResponse, ClearTextRequest, ClearTextResponse, ComposingText,
    MoveCursorRequest, MoveCursorResponse, RemoveTextRequest, RemoveTextResponse,
    ShrinkTextRequest, ShrinkTextResponse,
};

use crate::candidate_pipeline::{self, StageInput};
use crate::plugin_client::PluginClient;
use crate::session::session_of;
use crate::wrappers::{
    RawComposingText, add_text, clear_text, get_composed_text, load_config, move_cursor,
    remove_text, set_context, shrink_text,
};

pub struct MyAzookeyService {
    plugins: PluginClient,
}

/// Ceiling on one `RemoveText` batch.
///
/// The client already clamps the count to the reading it can see, so a batch
/// this large means the two sides disagree — a stale TIP, or a repeat count a
/// host inflated. Deleting from an empty reading is harmless on the engine
/// side, so the cap is not about correctness; it bounds how long one FFI call
/// can hold the single-threaded runtime, which is what a health ping and every
/// other application's keystrokes are waiting on.
const MAX_REMOVE_TEXT_COUNT: i32 = 512;

impl MyAzookeyService {
    pub fn new() -> Self {
        MyAzookeyService {
            plugins: PluginClient::new(),
        }
    }

    /// Builds the ComposingText payload every composing-text RPC returns:
    /// the current hiragana plus a fresh candidate fetch for the session,
    /// run through the candidate pipeline. Every RPC that returns
    /// candidates goes through here, so the pipeline hook lives in this
    /// one place.
    ///
    /// The plugin host is asked BEFORE the pipeline runs and its answer is
    /// carried in as data, which is what keeps the pipeline a synchronous
    /// pure function. Awaiting here does not stall the single-threaded
    /// runtime — only the FFI does that — and with plugins off (the
    /// default) there is nothing to await at all.
    async fn composed(&self, session: i64, composing_text: RawComposingText) -> ComposingText {
        let hiragana = composing_text.text;
        let candidates = get_composed_text(session);
        let offered = self.plugins.offer(&hiragana, &candidates).await;
        // The host and the validator are given the same view of what the
        // engine produced: this list, before the pipeline removes
        // anything from it.
        let input = StageInput::new(&hiragana, &offered, &candidates);
        let suggestions = candidate_pipeline::run(&input, candidates);
        ComposingText {
            hiragana,
            suggestions,
        }
    }
}

/// The engine wants the text immediately left of the caret on the current
/// line as conversion context. Take the last non-empty line, splitting on BOTH
/// `\r` and `\n`: splitting on `\r` alone left a leading `\n` on the segment in
/// CRLF documents, feeding a stray newline into the engine's left-side context.
fn last_line_context(context: &str) -> &str {
    context
        .split(['\r', '\n'])
        .rfind(|s| !s.is_empty())
        .unwrap_or_default()
}

#[tonic::async_trait]
impl AzookeyService for MyAzookeyService {
    async fn append_text(
        &self,
        request: Request<AppendTextRequest>,
    ) -> Result<Response<AppendTextResponse>, Status> {
        let session = session_of(&request);
        let input = request.into_inner().text_to_append;

        Ok(Response::new(AppendTextResponse {
            composing_text: Some(self.composed(session, add_text(session, &input)).await),
        }))
    }

    async fn remove_text(
        &self,
        request: Request<RemoveTextRequest>,
    ) -> Result<Response<RemoveTextResponse>, Status> {
        let session = session_of(&request);
        let count = request.into_inner().count.clamp(1, MAX_REMOVE_TEXT_COUNT);

        // The loop is the cheap half: RemoveText drops one kana from the
        // reading and does no conversion. `composed` is the expensive one — it
        // reconverts the whole reading, Zenzai inference included — so it runs
        // once for the batch rather than once per kana. That is the entire
        // point of the count.
        let mut composing_text = remove_text(session);
        for _ in 1..count {
            composing_text = remove_text(session);
        }

        Ok(Response::new(RemoveTextResponse {
            composing_text: Some(self.composed(session, composing_text).await),
        }))
    }

    async fn move_cursor(
        &self,
        request: Request<MoveCursorRequest>,
    ) -> Result<Response<MoveCursorResponse>, Status> {
        let session = session_of(&request);
        let offset = request.into_inner().offset;

        Ok(Response::new(MoveCursorResponse {
            composing_text: Some(self.composed(session, move_cursor(session, offset)).await),
        }))
    }

    async fn clear_text(
        &self,
        request: Request<ClearTextRequest>,
    ) -> Result<Response<ClearTextResponse>, Status> {
        let session = session_of(&request);
        clear_text(session);
        Ok(Response::new(ClearTextResponse {}))
    }

    async fn shrink_text(
        &self,
        request: Request<ShrinkTextRequest>,
    ) -> Result<Response<ShrinkTextResponse>, Status> {
        let session = session_of(&request);
        let surface_offset = request.into_inner().surface_offset;

        // Committing a candidate that spends no kana is something the client
        // never has reason to ask for — the candidate it just confirmed always
        // covers at least one kana of the reading. In practice a zero means a
        // TIP older than the switch of this count from keystrokes to kana: it
        // sends its number in proto field 1, retired when `surface_offset`
        // was renumbered to 2, so nothing arrives here. The reading then
        // survives the commit and the next preview re-converts the whole thing
        // — on screen the confirmed clause looks duplicated (せいちょうせんりゃく
        // committed as 政庁 came back as 政庁成長戦略). The skew is silent
        // otherwise, because an application that was already running keeps the
        // DLL it loaded even after the new one is registered. A warning, not an
        // error: the old TIP is still usable, just wrong at this one boundary.
        if surface_offset <= 0 {
            tracing::warn!(
                surface_offset,
                "ShrinkText spends no kana, so the reading survives the commit. \
                 This is what a TIP built before the kana-unit shrink looks \
                 like: re-register the current DLL and restart the application \
                 that is typing."
            );
        }

        Ok(Response::new(ShrinkTextResponse {
            composing_text: Some(
                self.composed(session, shrink_text(session, surface_offset))
                    .await,
            ),
        }))
    }

    async fn set_context(
        &self,
        request: Request<shared::proto::SetContextRequest>,
    ) -> Result<Response<shared::proto::SetContextResponse>, Status> {
        let session = session_of(&request);
        let context = request.into_inner().context;

        set_context(session, last_line_context(&context));
        Ok(Response::new(shared::proto::SetContextResponse {}))
    }

    async fn update_config(
        &self,
        _: Request<shared::proto::UpdateConfigRequest>,
    ) -> Result<Response<shared::proto::UpdateConfigResponse>, Status> {
        // ONE read, here, handed to both halves. The engine used to open
        // settings.json for itself, so this RPC read the file twice and a save
        // landing between the two reads applied half of each version.
        let config = shared::AppConfig::try_read().map_err(Status::failed_precondition)?;
        let json = config
            .to_engine_json()
            .map_err(|e| Status::internal(format!("could not pass the settings on: {e}")))?;

        // Reported, not swallowed. A decode failure in the engine used to stay
        // there: the settings app showed a success toast while conversion
        // carried on with the values it already had.
        if !load_config(&json) {
            return Err(Status::internal(
                "the conversion engine refused the new settings and kept the ones it had",
            ));
        }
        // the same signal reaches the Rust side: `plugins.enable` lives in
        // settings.json too, and the engine's reload does not carry it
        self.plugins.reload_config();
        Ok(Response::new(shared::proto::UpdateConfigResponse {}))
    }
}

#[cfg(test)]
mod tests {
    use super::last_line_context;

    /// `UpdateConfig` has to reload BOTH halves of the settings file, and
    /// nothing was checking that it still did.
    ///
    /// The engine gets the zenzai keys over the FFI; `plugins.enable` is read
    /// on this side, because the engine's reload does not carry it. Deleting
    /// either call left the whole suite green — the engine half needs a live
    /// engine to observe, and the plugin half needs a live host — while the
    /// symptom for a user was a setting that saved and then did nothing until
    /// they restarted.
    ///
    /// So the wiring is read instead, the same way `installer_config.rs` reads
    /// the installer's. It cannot tell whether the calls WORK; it can tell
    /// that both are still there, which is the failure that actually happened.
    #[test]
    fn update_config_reloads_the_engine_and_the_plugin_hook() {
        const SOURCE: &str = include_str!("service.rs");

        let start = SOURCE
            .find("async fn update_config")
            .expect("service.rs should implement update_config");
        let rest = &SOURCE[start..];
        // to the next method, or the end of the impl
        let end = rest[1..]
            .find("\n    async fn ")
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        assert!(
            body.contains("load_config(&json)"),
            "update_config must hand the settings to the engine: got {body}"
        );
        assert!(
            body.contains("self.plugins.reload_config()"),
            "update_config must also re-read plugins.enable, which the engine's \
             reload does not carry: got {body}"
        );
        assert!(
            body.contains("try_read"),
            "the document must be read ONCE here and passed on, rather than \
             read again on the far side of the FFI boundary: got {body}"
        );
    }

    #[test]
    fn last_line_context_takes_the_final_nonempty_line() {
        assert_eq!(last_line_context("前の行\r\n現在の行"), "現在の行");
        assert_eq!(last_line_context("前の行\n現在の行"), "現在の行");
        assert_eq!(last_line_context("前の行\r現在の行"), "現在の行");
        // no stray newline survives on a CRLF boundary
        assert_eq!(last_line_context("a\r\nb"), "b");
        // trailing newlines fall back to the previous non-empty line
        assert_eq!(last_line_context("最後\r\n"), "最後");
        // single line and empty input
        assert_eq!(last_line_context("ただ一行"), "ただ一行");
        assert_eq!(last_line_context(""), "");
        assert_eq!(last_line_context("\r\n\r\n"), "");
    }
}
