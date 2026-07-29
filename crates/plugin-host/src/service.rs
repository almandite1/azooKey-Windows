//! The gRPC surface: version check, dispatch to the builtins, answer.

use tonic::{Request, Response, Status};

use shared::proto::plugin_host_service_server::PluginHostService;
use shared::proto::{ProcessCandidatesRequest, ProcessCandidatesResponse};

use crate::builtin;

/// The plugin API version this host implements.
pub(crate) const API_VERSION: u32 = 1;

#[derive(Debug, Default)]
pub struct MyPluginHost;

#[tonic::async_trait]
impl PluginHostService for MyPluginHost {
    async fn process_candidates(
        &self,
        request: Request<ProcessCandidatesRequest>,
    ) -> Result<Response<ProcessCandidatesResponse>, Status> {
        let request = request.into_inner();

        // A version we do not implement is answered with "nothing to add"
        // rather than an error. The caller is fail-open either way, so both
        // routes end at the same candidate list — but an error would be
        // logged as a fault on every keystroke, and a version skew between
        // two binaries of the same product is a deployment state, not a
        // fault. Warned once per request at the caller's discretion, not
        // here: this runs per keystroke.
        if request.api_version != API_VERSION {
            return Ok(Response::new(ProcessCandidatesResponse::default()));
        }

        Ok(Response::new(ProcessCandidatesResponse {
            added: builtin::run(&request.reading, &request.candidates),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{API_VERSION, MyPluginHost};
    use shared::proto::plugin_host_service_server::PluginHostService as _;
    use shared::proto::{PluginCandidate, ProcessCandidatesRequest};

    fn request(api_version: u32, reading: &str) -> tonic::Request<ProcessCandidatesRequest> {
        tonic::Request::new(ProcessCandidatesRequest {
            api_version,
            reading: reading.to_string(),
            candidates: vec![PluginCandidate {
                text: "今日".to_string(),
                subtext: String::new(),
                corresponding_count: 4,
                surface_count: 3,
            }],
        })
    }

    #[tokio::test]
    async fn a_known_version_gets_the_builtins() {
        let response = MyPluginHost
            .process_candidates(request(API_VERSION, "きょう"))
            .await
            .expect("the call succeeds");

        assert!(
            !response.into_inner().added.is_empty(),
            "きょう has a date to offer"
        );
    }

    /// A caller from a future build gets an empty answer, not an error:
    /// the whole path is fail-open, and a version skew is a deployment
    /// state rather than a fault to report on every keystroke.
    #[tokio::test]
    async fn an_unknown_version_is_answered_with_nothing() {
        let response = MyPluginHost
            .process_candidates(request(API_VERSION + 1, "きょう"))
            .await
            .expect("an unknown version is still a successful call");

        assert!(response.into_inner().added.is_empty());
    }

    /// The same call over the real transport: our hand-written pipe
    /// listener, the generated server, the `shared::pipe` client connector
    /// the conversion server will use. Everything below `main` except the
    /// pipe NAME, and it needs no external process, so it runs in ordinary
    /// CI rather than waiting for someone to start a host.
    #[tokio::test]
    async fn the_service_answers_over_a_named_pipe() {
        use shared::proto::plugin_host_service_client::PluginHostServiceClient;

        // pipe names are machine-global, so keep this run's name to itself
        let base = format!("azookey_test_plugin_{}", std::process::id());

        let incoming = azookey_server::TonicNamedPipeServer::new(&base)
            .expect("failed to create the pipe listener");
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    shared::proto::plugin_host_service_server::PluginHostServiceServer::new(
                        MyPluginHost,
                    ),
                )
                .serve_with_incoming(incoming)
                .await
        });

        let channel = shared::pipe::lazy_pipe_channel(format!(r"\\.\pipe\{base}"))
            .expect("failed to build the pipe channel");
        let mut client = PluginHostServiceClient::new(channel);

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.process_candidates(request(API_VERSION, "きょう").into_inner()),
        )
        .await
        .expect("the call timed out over the pipe transport")
        .expect("the call failed over the pipe transport");

        let added = response.into_inner().added;
        assert_eq!(added.len(), 3, "きょう has three date formats to offer");
        assert!(added.iter().all(|c| c.subtext == "日付"));
        // the span the caller advertised comes back on every addition
        assert!(
            added
                .iter()
                .all(|c| c.corresponding_count == 4 && c.surface_count == 3)
        );

        server.abort();
    }

    #[tokio::test]
    async fn an_ordinary_reading_gets_nothing() {
        let response = MyPluginHost
            .process_candidates(request(API_VERSION, "きしゃ"))
            .await
            .expect("the call succeeds");

        assert!(response.into_inner().added.is_empty());
    }
}
