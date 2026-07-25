use std::sync::Arc;
use std::time::{Duration, Instant};

use azookey_server::TonicNamedPipeServer;
use ipc::{WindowAction, WindowController, WindowService};
use shared::proto::window_service_server::WindowServiceServer;
use tao::dpi::LogicalSize;
use tao::platform::windows::EventLoopBuilderExtWindows;
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tonic::transport::Server;
use uiaccess::prepare_uiaccess_token;
use wry::WebContext;

pub mod candidate;
pub mod indicator;
pub mod ipc;
pub mod uiaccess;
pub mod utils;
pub mod window;
pub mod window_actions;

#[derive(Debug)]
pub enum UserEvent {
    UpdateHeight(i32),
    UpdateCandidates(String),
    UpdateSelection(i32),
    UpdateInputMethod(String),
    WindowAction(WindowAction),
    /// liveness probe: proves the event loop is still processing events
    Heartbeat,
    /// the grace period a deferred Show is waiting on has elapsed (issue #59)
    ShowDeadline,
}

/// Where WebView2 keeps its profile (issue #54). The decision itself lives
/// in `utils::webview2_data_dir_in` so it can be tested without the
/// environment; this only resolves the root.
fn webview2_data_dir() -> std::path::PathBuf {
    utils::webview2_data_dir_in(shared::local_data_root())
}

/// how often the event loop's liveness is probed
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// no processed heartbeat for this long = the event loop is stalled
const HEARTBEAT_STALL_THRESHOLD: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // obtain uiaccess token (on success this re-executes the process and
    // never returns: the original process stays behind as a supervision shim
    // that mirrors the child's exit code, so the launcher's watchdog keeps
    // covering the real UI). Without UIAccess the candidate window may appear
    // behind full-screen or elevated applications, but a working IME beats
    // no candidate window at all — continue instead of dying.
    if let Err(e) = prepare_uiaccess_token() {
        eprintln!("UIAccess unavailable ({e}); continuing without it");
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event()
        .with_any_thread(true)
        .build();

    // initialize window controller
    let (tx, mut rx) = mpsc::channel(32);
    let window_controller = WindowController::new(tx.clone());
    // who the visible window belongs to; shared with the disconnect watcher
    let show_owner = Arc::new(std::sync::Mutex::new(utils::ShowOwner::default()));
    let grpc_service = WindowService {
        controller: window_controller.clone(),
        show_owner: show_owner.clone(),
    };

    // start grpc server. Hide only ever arrives as an RPC, so an application
    // killed mid-composition would leave its candidate window on screen with
    // nobody left to take it down; the connection ending is the only notice
    // we get, and tonic gives it to us by dropping the pipe (issue #67).
    let (disconnect_tx, mut disconnect_rx) = mpsc::unbounded_channel();
    // normally the session's own name; `--pipe-base` lets the display tests
    // run a second ui.exe beside the installed one (see the function's note on
    // why only this end honours an override)
    let pipe_base = utils::pipe_base_from_args(std::env::args().skip(1))
        .unwrap_or_else(shared::pipe::ui_pipe_base);
    let incoming = TonicNamedPipeServer::with_disconnect_notify(&pipe_base, disconnect_tx)?;
    // health service for the launcher's watchdog. The reported status
    // follows the EVENT LOOP's liveness (via the heartbeat below), so a
    // stalled window loop turns the whole process NOT_SERVING even while
    // the tokio runtime is fine — the launcher then restarts us.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    tokio::spawn(async move {
        println!("WindowServer listening");
        let result = Server::builder()
            .add_service(health_service)
            .add_service(WindowServiceServer::new(grpc_service))
            .serve_with_incoming(incoming)
            .await;

        // without IPC this process is a zombie (window alive, unreachable);
        // exit so the launcher can restart it
        eprintln!("gRPC server terminated: {:?}", result);
        std::process::exit(1);
    });

    // a dropped connection hides the window, but only when it is the one
    // that showed it: a connection can be reported dead after the next
    // application has already put its own candidates up, and hiding then
    // would blank a live composition somewhere else. Goes down the ordinary
    // Hide path so the placement bookkeeping (issue #59) stays consistent.
    {
        let controller = window_controller.clone();
        let show_owner = show_owner.clone();
        tokio::spawn(async move {
            while let Some(session) = disconnect_rx.recv().await {
                let owns = show_owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .on_disconnect(session);
                if owns {
                    eprintln!("client {session} disconnected while showing; hiding");
                    if let Err(e) = controller.dispatch(WindowAction::Hide).await {
                        eprintln!("hide after disconnect failed: {e}");
                    }
                }
            }
        });
    }

    let event_loop_proxy = event_loop.create_proxy();
    let task_guard: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    // One context for both webviews: they then share a single WebView2
    // environment, which is also what keeps their environment options
    // identical (WebView2 rejects two environments over one user data folder
    // when the options differ). Declared before the windows so the builders
    // can borrow it and the windows for the same region.
    let mut web_context = WebContext::new(Some(webview2_data_dir()));

    let proxy_clone = event_loop_proxy.clone();
    let candidate_window = candidate::create_candidate_window(&event_loop)?;
    let candidate_webview_builder = candidate::create_candidate_webview(&mut web_context)?;
    let candidate_webview = candidate_webview_builder
        .with_devtools(true)
        .with_ipc_handler(move |message| {
            if let Ok(message) = serde_json::from_str::<serde_json::Value>(message.body())
                && let Some(type_value) = message.get("type")
                && type_value == "resize"
                && let Some(height) = message.get("height")
            {
                let height = height.as_f64().unwrap_or(0.0);
                let _ = proxy_clone.send_event(UserEvent::UpdateHeight(height as i32));
            }
        })
        .build(&candidate_window)?;

    let indicator_window = indicator::create_indicator_window(&event_loop)?;
    // the candidate builder's borrow of web_context ended when it was built
    // above (the returned WebView holds no lifetime), so it can be lent again
    let indicator_webview =
        indicator::create_indicator_webview(&indicator_window, &mut web_context)?;

    // handle window actions
    let proxy_clone = event_loop_proxy.clone();
    tokio::spawn(async move {
        while let Some(action) = rx.recv().await {
            // send_event only fails when the event loop is gone
            if proxy_clone
                .send_event(UserEvent::WindowAction(action))
                .is_err()
            {
                break;
            }
        }
    });

    // event-loop liveness: periodically post a Heartbeat event and reflect
    // whether the loop processed a recent one in the health status
    let last_beat = Arc::new(std::sync::Mutex::new(Instant::now()));
    {
        let last_beat = last_beat.clone();
        let proxy = event_loop_proxy.clone();
        tokio::spawn(async move {
            health_reporter
                .set_service_status("", tonic_health::ServingStatus::Serving)
                .await;

            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                let _ = proxy.send_event(UserEvent::Heartbeat);

                let stalled = last_beat
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .elapsed()
                    > HEARTBEAT_STALL_THRESHOLD;
                let status = if stalled {
                    eprintln!("event loop has not processed a heartbeat recently");
                    tonic_health::ServingStatus::NotServing
                } else {
                    tonic_health::ServingStatus::Serving
                };
                health_reporter.set_service_status("", status).await;
            }
        });
    }

    // where the candidate window may appear: the caret rect the TIP last
    // reported (kept so resizes can re-clamp the grown window into the work
    // area, issue #3) plus whether that rect belongs to the composition being
    // shown (issue #59)
    let mut placement = utils::CandidatePlacement::default();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::ScaleFactorChanged { new_inner_size, .. },
                window_id,
                ..
            } if window_id == candidate_window.id() => {
                // crossing into a monitor with a different DPI changes the
                // window's physical size; re-clamp with the size it is about
                // to have (inner_size() still reports the old one here)
                window_actions::reposition_candidate(
                    &candidate_window,
                    &placement.caret,
                    new_inner_size.width as i32,
                    new_inner_size.height as i32,
                );
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // The candidate and indicator windows are overlays with no
                // user-facing close affordance. Ignore CloseRequested (e.g. an
                // external WM_CLOSE from Task Manager's "End task"): exiting
                // here returns success, which the launcher's supervisor reads
                // as a clean shutdown and stops restarting — leaving every app
                // without a candidate window. The launcher owns ui.exe's
                // lifecycle and tears it down via the job object when needed.
            }
            Event::UserEvent(script) => match script {
                UserEvent::UpdateCandidates(candidates) => {
                    if let Err(e) = candidate_webview
                        .evaluate_script(&format!("updateCandidates({})", candidates))
                    {
                        eprintln!("evaluate_script failed: {e}");
                    }
                }
                UserEvent::UpdateSelection(index) => {
                    if let Err(e) =
                        candidate_webview.evaluate_script(&format!("updateSelection({})", index))
                    {
                        eprintln!("evaluate_script failed: {e}");
                    }
                }
                UserEvent::UpdateInputMethod(input_method) => {
                    // quoted and escaped through serde_json (see the function's
                    // comment): the mode string is untrusted RPC input
                    if let Err(e) = indicator_webview
                        .evaluate_script(&utils::update_input_method_script(&input_method))
                    {
                        eprintln!("evaluate_script failed: {e}");
                    }
                }
                UserEvent::UpdateHeight(height) => {
                    // the webview reports CSS px (logical); the width must be
                    // logical too. Feeding the PHYSICAL inner width into a
                    // LogicalSize grew the window by the scale factor on
                    // every resize at high DPI (B20).
                    let scale = candidate_window.scale_factor();
                    let width = candidate_window.inner_size().to_logical::<f64>(scale).width;
                    let new_size = LogicalSize::new(width, height as f64);
                    candidate_window.set_inner_size(new_size);

                    // a taller list can now overflow the work-area bottom (or
                    // flip and overflow the top); re-clamp for the size the
                    // window is GOING to have (issue #3)
                    let physical = new_size.to_physical::<i32>(scale);
                    window_actions::reposition_candidate(
                        &candidate_window,
                        &placement.caret,
                        physical.width,
                        physical.height,
                    );

                    // the window is now the size it is going to be, so a Show
                    // that was waiting on the measurement can be honoured
                    // (issue #59)
                    if placement.on_height() {
                        window_actions::show_candidate(&candidate_window);
                    }
                }
                UserEvent::ShowDeadline => {
                    // the position never came (a host that reports no layout
                    // for the range). Candidates in a stale spot still beat
                    // no candidates at all (issue #59).
                    if placement.on_deadline() {
                        window_actions::show_candidate(&candidate_window);
                    }
                }
                UserEvent::Heartbeat => {
                    // test hook: simulate a stalled event loop (inert unless
                    // the env var is set)
                    if std::env::var_os("AZOOKEY_TEST_BLOCK_EVENT_LOOP").is_some() {
                        eprintln!("TEST MODE: blocking the event loop now");
                        std::thread::sleep(std::time::Duration::MAX);
                    }
                    *last_beat
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
                }
                UserEvent::WindowAction(action) => {
                    window_actions::handle_window_action(
                        action,
                        &candidate_window,
                        &indicator_window,
                        &task_guard,
                        &event_loop_proxy,
                        &mut placement,
                    );
                }
            },
            _ => (),
        }
    });
}
