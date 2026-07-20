use std::sync::Arc;
use std::time::{Duration, Instant};

use azookey_server::TonicNamedPipeServer;
use ipc::{WindowAction, WindowController, WindowService};
use shared::proto::window_service_server::WindowServiceServer;
use tao::dpi::LogicalSize;
use tao::platform::windows::EventLoopBuilderExtWindows;
use tao::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tonic::transport::Server;
use uiaccess::prepare_uiaccess_token;

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
}

/// how often the event loop's liveness is probed
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// no processed heartbeat for this long = the event loop is stalled
const HEARTBEAT_STALL_THRESHOLD: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // obtain uiaccess token (on success this re-executes the process and
    // never returns). Without UIAccess the candidate window may appear
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
    let grpc_service = WindowService {
        controller: window_controller.clone(),
    };

    // start grpc server
    let incoming = TonicNamedPipeServer::new(&shared::pipe::ui_pipe_base())?;
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

    let event_loop_proxy = event_loop.create_proxy();
    let task_guard: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    let proxy_clone = event_loop_proxy.clone();
    let candidate_window = candidate::create_candidate_window(&event_loop)?;
    let candidate_webview_builder = candidate::create_candidate_webview()?;
    let candidate_webview = candidate_webview_builder
        .with_devtools(true)
        .with_ipc_handler(move |message| {
            if let Ok(message) = serde_json::from_str::<serde_json::Value>(message.body()) {
                if let Some(type_value) = message.get("type") {
                    if type_value == "resize" {
                        if let Some(height) = message.get("height") {
                            let height = height.as_f64().unwrap_or(0.0);
                            let _ = proxy_clone.send_event(UserEvent::UpdateHeight(height as i32));
                        }
                    }
                }
            }
        })
        .build(&candidate_window)?;

    let indicator_window = indicator::create_indicator_window(&event_loop)?;
    let indicator_webview = indicator::create_indicator_webview(&indicator_window)?;

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
        let mut health_reporter = health_reporter;
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

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {}
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
                    // Serialize through serde_json so the value becomes a
                    // properly quoted/escaped JS string literal, exactly like
                    // updateCandidates above. The mode string ("あ"/"A") comes
                    // over the SetInputMode RPC, which any local process on the
                    // pipe can call with an arbitrary payload; a raw `"{}"`
                    // interpolation let a `"`/`\` break out of the string and
                    // inject script into the (UIAccess) webview.
                    let arg =
                        serde_json::to_string(&input_method).unwrap_or_else(|_| "\"\"".to_string());
                    if let Err(e) =
                        indicator_webview.evaluate_script(&format!("updateInputMethod({arg})"))
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
                    candidate_window.set_inner_size(LogicalSize::new(width, height as f64));
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
                    );
                }
            },
            _ => (),
        }
    });
}
