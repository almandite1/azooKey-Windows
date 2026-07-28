use std::sync::{Arc, Mutex, PoisonError};

use azookey_server::PipeConnectInfo;
use shared::proto::{
    EmptyResponse, SetInputModeRequest, UpdateCandidateViewRequest,
    window_service_server::WindowService as WindowServiceProto,
};

use crate::geometry::CaretRect;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

#[derive(Debug, Clone)]
pub struct WindowController {
    sender: mpsc::Sender<WindowAction>,
}

impl WindowController {
    pub fn new(sender: mpsc::Sender<WindowAction>) -> Self {
        Self { sender }
    }

    /// Forwards an action to the window event loop. Returns a gRPC error
    /// instead of panicking when the event loop side has shut down.
    ///
    /// `pub(crate)` so a dropped connection can hide the window down exactly
    /// this path (see main.rs): the placement bookkeeping behind `Hide`
    /// (issue #59) only holds if every hide is the same hide.
    pub(crate) async fn dispatch(&self, action: WindowAction) -> Result<(), Status> {
        self.sender
            .send(action)
            .await
            .map_err(|e| Status::internal(format!("window event loop is gone: {e}")))
    }
}

// ウィンドウ操作コマンド
#[derive(Debug)]
pub enum WindowAction {
    Show,
    Hide,
    /// One update to what the window shows. Each field is `None` when the TIP
    /// left it out, meaning "keep what is there" — see
    /// `UpdateCandidateViewRequest` in window.proto.
    Update {
        position: Option<CaretRect>,
        candidates: Option<Vec<String>>,
        selection: Option<i32>,
    },
    SetInputMode(String),
}

#[derive(Debug)]
pub struct WindowService {
    pub controller: WindowController,
    /// Which connection the visible window belongs to, so its death can take
    /// the window with it (issue #67). Shared with the task that watches for
    /// disconnects; a std Mutex because nothing awaits while it is held.
    pub show_owner: Arc<Mutex<crate::placement::ShowOwner>>,
}

#[tonic::async_trait]
impl WindowServiceProto for WindowService {
    async fn show_window(
        &self,
        request: Request<EmptyResponse>,
    ) -> Result<Response<EmptyResponse>, Status> {
        // tonic put the accepted connection's id in the extensions; remember
        // whose window this is before it goes up
        let session = request
            .extensions()
            .get::<PipeConnectInfo>()
            .map(|info| info.session_id);
        self.show_owner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .on_show(session);

        self.controller.dispatch(WindowAction::Show).await?;
        Ok(Response::new(EmptyResponse {}))
    }

    async fn hide_window(
        &self,
        _request: Request<EmptyResponse>,
    ) -> Result<Response<EmptyResponse>, Status> {
        self.show_owner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .on_hide();

        self.controller.dispatch(WindowAction::Hide).await?;
        Ok(Response::new(EmptyResponse {}))
    }
    /// The list, the highlight and the caret rect, in whatever combination
    /// the TIP had news about. An update with nothing set is legal and does
    /// nothing; it is not worth an error, and refusing it would make the
    /// caller's life harder for no gain.
    async fn update_candidate_view(
        &self,
        request: Request<UpdateCandidateViewRequest>,
    ) -> Result<Response<EmptyResponse>, Status> {
        let update = request.into_inner();

        self.controller
            .dispatch(WindowAction::Update {
                position: update.position.map(|position| CaretRect {
                    top: position.top,
                    left: position.left,
                    bottom: position.bottom,
                    right: position.right,
                }),
                // the wrapper message is what distinguishes "no news about
                // the list" from "the list is now empty"
                candidates: update.candidates.map(|list| list.texts),
                selection: update.selection,
            })
            .await?;

        Ok(Response::new(EmptyResponse {}))
    }

    async fn set_input_mode(
        &self,
        request: Request<SetInputModeRequest>,
    ) -> Result<Response<EmptyResponse>, Status> {
        let mode = request.into_inner().mode;
        self.controller
            .dispatch(WindowAction::SetInputMode(mode))
            .await?;

        Ok(Response::new(EmptyResponse {}))
    }
}
