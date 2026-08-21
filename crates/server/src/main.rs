mod candidate_pipeline;
mod ffi;
mod memory_dir;
mod plugin_client;
mod service;
mod session;
mod trace;
mod wrappers;

use std::time::Instant;

use azookey_server::TonicNamedPipeServer;
use tonic::transport::Server;
use tonic_reflection::server::Builder as ReflectionBuilder;

use shared::proto::azookey_service_server::AzookeyServiceServer;

use service::MyAzookeyService;

// The Swift engine keeps @MainActor global state and its FFI exports are not
// thread-safe. A single-threaded runtime serializes every FFI call onto one
// OS thread; the default multi-threaded runtime crashes inside dispatch.dll.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    trace::setup_logger();
    tracing::info!("AzookeyServer started");
    // Everything between here and "listening" is dead time: the pipe does not
    // exist until serve_with_incoming below, so a keystroke arriving in this
    // window has nothing to talk to. It is also where issue #108 spends its
    // seconds, and the only number anyone had for it was the engine's own
    // "converter warm-up took" -- which covers the last fraction of it. On one
    // machine the warm-up ran 0.8s inside a window that ran 8.05s, so the
    // measurement everyone was reading described a twelfth of the problem.
    // Time each phase so a log says which one grew.
    let startup = Instant::now();
    // get executable directory
    let current_exe = std::env::current_exe()?;
    let parent_dir = current_exe
        .parent()
        .ok_or("executable path has no parent directory")?;
    // Configuration BEFORE Initialize, and from here rather than from the
    // engine: Initialize warms the converter up using the current options, so
    // a session that starts with Zenzai on has to know that before the warm-up
    // rather than on the first keystroke after it. The engine no longer opens
    // settings.json at all — this process is its only reader.
    //
    // Before that read, though: the settings document carries the learning
    // directory, and the engine only adopts it if it already EXISTS (it must
    // never create it — see memory_dir.rs for why the permissions have to be
    // ours). Initialize's warm-up conversion is the first thing to build a
    // learning store, so the directory has to be there and locked down by
    // now.
    memory_dir::ensure_secured_memory_dir();
    match shared::AppConfig::new().to_engine_json() {
        Ok(json) => {
            if !wrappers::load_config(&json) {
                tracing::warn!("the engine refused the stored settings; using its own defaults");
            }
        }
        Err(e) => tracing::warn!("could not pass the stored settings to the engine: {e}"),
    }
    let settings_done = startup.elapsed();

    // Announced before the call rather than only timed after it: when the
    // engine hangs in here the watchdog kills the process, the line below
    // never runs, and the log would otherwise not say which phase it died in.
    tracing::info!("startup: initializing the engine");
    wrappers::initialize(&parent_dir.to_string_lossy());
    let engine_done = startup.elapsed();

    let service = MyAzookeyService::new();

    // standard gRPC health service, polled by the launcher's watchdog.
    // Because this runtime is single-threaded, ANY hang inside a Swift FFI
    // call also blocks the health check — a trivial ping detects engine
    // hangs without touching the engine.
    // "" is the gRPC health protocol's "overall server" service name; the
    // launcher checks that instead of a per-service name
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    // test hook for the watchdog: block the (single-threaded) runtime after
    // N seconds so every RPC — including the health check — stops answering.
    // Completely inert unless the env var is set.
    if let Some(secs) = std::env::var("AZOOKEY_TEST_HANG_AFTER_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        tracing::warn!("TEST MODE: runtime will hang after {secs}s");
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            tracing::warn!("TEST MODE: blocking the runtime now");
            std::thread::sleep(std::time::Duration::MAX);
        });
    }

    // Phases, not a single total: the total is already derivable from the two
    // timestamps around it, and the point is which phase owns the seconds.
    // "settings" is this process reading settings.json and handing it to the
    // engine; "engine initialize" is the FFI call, whose own split between
    // building the converter and warming it up the engine logs itself.
    tracing::info!(
        "startup phases: settings {:.2?}, engine initialize {:.2?}, service setup {:.2?}",
        settings_done,
        engine_done - settings_done,
        startup.elapsed() - engine_done
    );
    tracing::info!("AzookeyServer listening");

    Server::builder()
        .add_service(health_service)
        .add_service(AzookeyServiceServer::new(service))
        .add_service(
            ReflectionBuilder::configure()
                .register_encoded_file_descriptor_set(shared::proto::FILE_DESCRIPTOR_SET)
                .build_v1()?,
        )
        .serve_with_incoming(TonicNamedPipeServer::new(&shared::pipe::server_pipe_base())?)
        .await?;

    Ok(())
}
