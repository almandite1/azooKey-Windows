//! The gRPC service implementation: request/response plumbing only.
//! Engine access goes through the safe wrappers in wrappers.rs.

use tonic::{Request, Response, Status};

use shared::proto::azookey_service_server::AzookeyService;
use shared::proto::{
    AppendTextRequest, AppendTextResponse, ClearTextRequest, ClearTextResponse, ComposingText,
    MoveCursorRequest, MoveCursorResponse, RemoveTextRequest, RemoveTextResponse,
    ShrinkTextRequest, ShrinkTextResponse,
};

use crate::session::session_of;
use crate::wrappers::{
    add_text, clear_text, get_composed_text, load_config, move_cursor, remove_text, set_context,
    shrink_text, RawComposingText,
};

#[derive(Debug, Default)]
pub struct MyAzookeyService;

/// Builds the ComposingText payload every composing-text RPC returns: the
/// current hiragana plus a fresh candidate fetch for the session.
fn composed(session: i32, composing_text: RawComposingText) -> ComposingText {
    ComposingText {
        hiragana: composing_text.text,
        suggestions: get_composed_text(session),
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

        Ok(Response::new(RemoveTextResponse {
            composing_text: Some(composed(session, remove_text(session))),
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
        let offset = request.into_inner().offset;

        Ok(Response::new(ShrinkTextResponse {
            composing_text: Some(composed(session, shrink_text(session, offset))),
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
