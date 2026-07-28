//! The gRPC service implementation: request/response plumbing only.
//! Engine access goes through the safe wrappers in wrappers.rs.

use tonic::{Request, Response, Status};

use shared::proto::azookey_service_server::AzookeyService;
use shared::proto::{
    AppendTextRequest, AppendTextResponse, ClearTextRequest, ClearTextResponse, ComposingText,
    MoveCursorRequest, MoveCursorResponse, RemoveTextRequest, RemoveTextResponse,
    ShrinkTextRequest, ShrinkTextResponse,
};

use crate::candidate_pipeline;
use crate::session::session_of;
use crate::wrappers::{
    RawComposingText, add_text, clear_text, get_composed_text, load_config, move_cursor,
    remove_text, set_context, shrink_text,
};

#[derive(Debug, Default)]
pub struct MyAzookeyService;

/// Ceiling on one `RemoveText` batch.
///
/// The client already clamps the count to the reading it can see, so a batch
/// this large means the two sides disagree — a stale TIP, or a repeat count a
/// host inflated. Deleting from an empty reading is harmless on the engine
/// side, so the cap is not about correctness; it bounds how long one FFI call
/// can hold the single-threaded runtime, which is what a health ping and every
/// other application's keystrokes are waiting on.
const MAX_REMOVE_TEXT_COUNT: i32 = 512;

/// Builds the ComposingText payload every composing-text RPC returns: the
/// current hiragana plus a fresh candidate fetch for the session, run
/// through the candidate pipeline. Every RPC that returns candidates goes
/// through here, so the pipeline hook lives in this one place.
fn composed(session: i64, composing_text: RawComposingText) -> ComposingText {
    let hiragana = composing_text.text;
    let suggestions = candidate_pipeline::run(&hiragana, get_composed_text(session));
    ComposingText {
        hiragana,
        suggestions,
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
            composing_text: Some(composed(session, add_text(session, &input))),
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
            composing_text: Some(composed(session, composing_text)),
        }))
    }

    async fn move_cursor(
        &self,
        request: Request<MoveCursorRequest>,
    ) -> Result<Response<MoveCursorResponse>, Status> {
        let session = session_of(&request);
        let offset = request.into_inner().offset;

        Ok(Response::new(MoveCursorResponse {
            composing_text: Some(composed(session, move_cursor(session, offset))),
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
            composing_text: Some(composed(session, shrink_text(session, surface_offset))),
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
        load_config();
        Ok(Response::new(shared::proto::UpdateConfigResponse {}))
    }
}

#[cfg(test)]
mod tests {
    use super::last_line_context;

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
