mod candidate_pipeline;
mod ffi;
mod plugin_client;
mod service;
mod session;
mod trace;
mod wrappers;

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
    // get executable directory
    let current_exe = std::env::current_exe()?;
    let parent_dir = current_exe
        .parent()
        .ok_or("executable path has no parent directory")?;
    wrappers::initialize(&parent_dir.to_string_lossy());

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
