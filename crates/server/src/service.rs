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
        let trimmed_context = context
            .split('\r')
            .rfind(|s| !s.is_empty())
            .unwrap_or_default();

        set_context(session, trimmed_context);
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
