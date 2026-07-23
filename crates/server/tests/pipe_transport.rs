//! Transport-level test for the named-pipe gRPC listener.
//!
//! Unlike `ipc_smoke.rs`, this needs no Swift engine and no running server:
//! it stands up [`TonicNamedPipeServer`] on a throwaway pipe name, serves the
//! stock health service over it, and calls that service through the same
//! `shared::pipe` client connector the real client uses. What it covers is the
//! seam that every tonic upgrade puts at risk — our hand-written `Connected` /
//! `AsyncRead` / `AsyncWrite` transport and the `service_fn` client connector
//! have to keep satisfying tonic's traits *at run time*, not just compile.
//! That seam was previously only exercised by tests requiring a live engine.

use azookey_server::TonicNamedPipeServer;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;

/// Pipe names are machine-global, so keep this run's name to itself.
fn unique_pipe_base(tag: &str) -> String {
    format!("azookey_test_{}_{}", tag, std::process::id())
}

#[tokio::test]
async fn health_round_trips_over_the_named_pipe_transport() {
    let base = unique_pipe_base("health");

    let incoming = TonicNamedPipeServer::new(&base).expect("failed to create the pipe listener");

    let (reporter, health_service) = tonic_health::server::health_reporter();
    reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_incoming(incoming)
            .await
    });

    let channel = shared::pipe::lazy_pipe_channel(format!(r"\\.\pipe\{base}"))
        .expect("failed to build the pipe channel");
    let mut client = HealthClient::new(channel);

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.check(HealthCheckRequest {
            service: String::new(),
        }),
    )
    .await
    .expect("health check timed out over the pipe transport")
    .expect("health check failed over the pipe transport");

    assert_eq!(
        response.into_inner().status,
        tonic_health::pb::health_check_response::ServingStatus::Serving as i32,
        "the listener accepted the connection but reported the wrong status"
    );

    server.abort();
}

/// Connect-failure shape of server loss: the engine is not running, so opening
/// the pipe fails outright. That must reach the client as `Unavailable`, which
/// `IPCService::exec` tags as `ServerUnavailable` so `handle_action` resets the
/// composition.
///
/// Our connector returns a plain `io::Error`; the mapping to `Unavailable`
/// happens because tonic wraps every custom connector's error in
/// `tonic::ConnectError`, which its source-chain classifier recognises. That is
/// tonic's behaviour, not ours, so pin it here — an upgrade that stopped
/// wrapping would degrade this to a bare `Unknown` with no compile error.
#[tokio::test]
async fn a_call_to_a_missing_pipe_is_classified_unavailable() {
    let pipe_path = format!(r"\\.\pipe\{}", unique_pipe_base("missing"));

    let channel =
        shared::pipe::lazy_pipe_channel(pipe_path).expect("failed to build the pipe channel");
    let mut client = HealthClient::new(channel);

    let status = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.check(HealthCheckRequest {
            service: String::new(),
        }),
    )
    .await
    .expect("the call to a missing pipe hung instead of failing")
    .expect_err("a call to a nonexistent pipe must not succeed");

    assert_eq!(
        status.code(),
        tonic::Code::Unavailable,
        "a missing pipe must map to Unavailable so the client's server-loss \
         recovery fires; got {status:?}"
    );
    assert!(
        shared::pipe::is_transport_failure(&status),
        "a missing pipe must be classified as server loss; got {status:?}"
    );
}

/// The same classification, but through the path that actually happens in the
/// field: a channel that has already served an RPC, whose server then dies. The
/// server runs on its own runtime on its own thread so that dropping that
/// runtime tears down the accept loop *and* the established connection at once
/// — the in-process analogue of the engine process being killed (aborting a
/// spawned serve task would not do it; tonic's per-connection tasks outlive it).
#[tokio::test]
async fn a_call_after_the_server_dies_is_classified_as_server_loss() {
    let base = unique_pipe_base("server_death");
    let server_base = base.clone();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    let server_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build the server runtime");

        runtime.block_on(async move {
            let incoming = TonicNamedPipeServer::new(&server_base)
                .expect("failed to create the pipe listener");
            let (reporter, health_service) = tonic_health::server::health_reporter();
            reporter
                .set_service_status("", tonic_health::ServingStatus::Serving)
                .await;

            tokio::spawn(async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(health_service)
                    .serve_with_incoming(incoming)
                    .await;
            });

            // stay alive until the test says to die
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
        });

        // hard drop: no graceful shutdown, so the client sees the pipe vanish
        // the way it does when the engine crashes
        drop(runtime);
    });

    let channel = shared::pipe::lazy_pipe_channel(format!(r"\\.\pipe\{base}"))
        .expect("failed to build the pipe channel");
    let mut client = HealthClient::new(channel);

    // the listener is created on the server runtime, so poll until it is up
    // rather than sleeping a guessed interval
    let mut connected = false;
    for _ in 0..100 {
        if client
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await
            .is_ok()
        {
            connected = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(connected, "the test server never became reachable");

    let _ = stop_tx.send(());
    server_thread.join().expect("the server thread panicked");

    let status = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.check(HealthCheckRequest {
            service: String::new(),
        }),
    )
    .await
    .expect("the call after server death hung instead of failing")
    .expect_err("a call after the server died must not succeed");

    // NOT an assertion on the code: tonic cannot classify a broken established
    // connection and reports `Unknown` here. What has to hold is the client's
    // rule — this is the exact status that must trigger the composition reset.
    assert!(
        shared::pipe::is_transport_failure(&status),
        "the death of the server mid-session must be classified as server loss, \
         otherwise the client keeps typing into a reading the fresh server never \
         had; got {status:?}"
    );
}

/// The accept loop must keep serving after a client goes away: it replaces the
/// consumed pipe instance on every iteration, and getting that wrong leaves the
/// IME dead for every application started after the first one disconnects.
#[tokio::test]
async fn the_listener_accepts_a_second_client_after_the_first_drops() {
    let base = unique_pipe_base("reconnect");

    let incoming = TonicNamedPipeServer::new(&base).expect("failed to create the pipe listener");

    let (reporter, health_service) = tonic_health::server::health_reporter();
    reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_incoming(incoming)
            .await
    });

    let pipe_path = format!(r"\\.\pipe\{base}");

    for attempt in 0..2 {
        let channel = shared::pipe::lazy_pipe_channel(pipe_path.clone())
            .expect("failed to build the pipe channel");
        let mut client = HealthClient::new(channel);

        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.check(HealthCheckRequest {
                service: String::new(),
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("health check timed out on attempt {attempt}"))
        .unwrap_or_else(|e| panic!("health check failed on attempt {attempt}: {e}"));

        // dropping the client closes the connection, which is what forces the
        // accept loop to create the next pipe instance
    }

    server.abort();
}
