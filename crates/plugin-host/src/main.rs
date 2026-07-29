//! The plugin host: a separate process that answers "what would you add
//! to this candidate list?".
//!
//! Why a process of its own, when the work is a pure function over a list:
//! the conversion server runs a single-threaded runtime because the Swift
//! FFI demands it, so a plugin that hung inside it would stop answering
//! the watchdog's health ping and get the ENGINE restarted. And the TIP
//! DLL is loaded into every text application, where a crash takes the
//! host application down with it. Out here, a plugin that hangs is a
//! timeout the server rides out, and a plugin that crashes is a process
//! the launcher restarts while typing carries on.
//!
//! The launcher starts it as its third supervised child, and the
//! conversion server calls it once per keystroke while `plugins.enable`
//! is set. Started by hand it is a working host on its own.

mod builtin;
mod service;
mod trace;

use azookey_server::TonicNamedPipeServer;
use tonic::transport::Server;

use shared::proto::plugin_host_service_server::PluginHostServiceServer;

use service::MyPluginHost;

/// Turns a panic in a request handler into the death of this process.
///
/// tonic catches a handler panic inside the connection task, so without
/// this the host would stay up, keep answering health checks, and add
/// nothing to any candidate list ever again — alive to the supervisor,
/// useless to the user, and rediscovered by the server's breaker every
/// thirty seconds forever. Dying is strictly better: the supervisor
/// restarts it with backoff, and a plugin whose panic is deterministic
/// eventually exhausts the budget and is given up on, non-fatally.
///
/// Deliberately minimal. The hook runs while unwinding, and anything it
/// does that could itself panic would abort with the original cause lost.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        // not `panic::abort`: an ordinary exit code is what the
        // supervisor already knows how to read as "died abnormally"
        std::process::exit(1);
    }));
}

// Single-threaded to match the conversion server. Nothing here needs it —
// there is no FFI and no shared mutable state — but one runtime flavour
// across the stack means one less thing to re-derive when a process
// misbehaves. If a builtin ever needs real concurrency, this is the line
// to revisit, and the reason will be its own rather than inherited.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    trace::setup_logger();
    install_panic_hook();
    tracing::info!("PluginHost started");

    // standard gRPC health service, polled by the launcher's watchdog once
    // this process is supervised
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    // Test hook for the watchdog: block the runtime after N seconds so
    // every RPC — the health check included — stops answering. Completely
    // inert unless the variable is set.
    //
    // A DIFFERENT variable from the server's on purpose. The e2e harness
    // sets its one as a machine-scope variable that the launcher passes to
    // every child, so a shared name would arm both processes at once and
    // the watchdog scenario could no longer say which one it had proved.
    if let Some(secs) = std::env::var("AZOOKEY_TEST_PLUGIN_HANG_AFTER_SECS")
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

    // The pipe listener comes from the server crate, together with the
    // DACL it applies. Deliberately the same string as the other two
    // pipes: a sandboxed principal may connect but may not create an
    // instance, so it cannot stand in front of this host and read what is
    // being typed. Adding a pipe must not mean writing a new descriptor.
    let incoming = TonicNamedPipeServer::new(&shared::pipe::plugin_pipe_base())?;

    tracing::info!("PluginHost listening");

    Server::builder()
        .add_service(health_service)
        .add_service(PluginHostServiceServer::new(MyPluginHost))
        .serve_with_incoming(incoming)
        .await?;

    Ok(())
}
