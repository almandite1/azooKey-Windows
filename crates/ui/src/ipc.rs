use std::sync::{Arc, Mutex, PoisonError};

use azookey_server::PipeConnectInfo;
use shared::proto::{
    EmptyResponse, SetCandidateRequest, SetInputModeRequest, SetPositionRequest,
    SetSelectionRequest, window_service_server::WindowService as WindowServiceProto,
};
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
#[derive(Debug, serde::Serialize)]
pub enum WindowAction {
    Show,
    Hide,
    SetPosition {
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
    },
    SetSelection {
        index: i32,
    },
    SetCandidate {
        candidates: Vec<String>,
    },
    SetInputMode(String),
}

#[derive(Debug)]
pub struct WindowService {
    pub controller: WindowController,
    /// Which connection the visible window belongs to, so its death can take
    /// the window with it (issue #67). Shared with the task that watches for
    /// disconnects; a std Mutex because nothing awaits while it is held.
    pub show_owner: Arc<Mutex<crate::utils::ShowOwner>>,
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
    async fn set_window_position(
        &self,
        request: Request<SetPositionRequest>,
    ) -> Result<Response<EmptyResponse>, Status> {
        let position = request
            .into_inner()
            .position
            .ok_or_else(|| Status::invalid_argument("position is required"))?;
        self.controller
            .dispatch(WindowAction::SetPosition {
                top: position.top,
                left: position.left,
                bottom: position.bottom,
                right: position.right,
            })
            .await?;

        Ok(Response::new(EmptyResponse {}))
    }

    async fn set_candidate(
        &self,
        request: Request<SetCandidateRequest>,
    ) -> Result<Response<EmptyResponse>, Status> {
        let candidate = request.into_inner().candidates;

        self.controller
            .dispatch(WindowAction::SetCandidate {
                candidates: candidate,
            })
            .await?;

        Ok(Response::new(EmptyResponse {}))
    }

    async fn set_selection(
        &self,
        request: Request<SetSelectionRequest>,
    ) -> Result<Response<EmptyResponse>, Status> {
        let index = request.into_inner().index;
        self.controller
            .dispatch(WindowAction::SetSelection { index })
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
