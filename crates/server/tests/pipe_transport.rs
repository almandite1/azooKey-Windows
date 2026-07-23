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
