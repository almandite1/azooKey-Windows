//! End-to-end smoke test against a RUNNING azookey-server instance.
//!
//! The server needs its full runtime environment (Swift runtime, llama
//! DLLs, dictionary, zenz.gguf), so this cannot run in bare CI — start the
//! server manually (e.g. via the launcher or from build/) and run:
//!
//!     cargo test -p azookey-server -- --ignored --test-threads=1
//!
//! (single-threaded: the tests share the server's global composing state)
//!
//! It exercises the Swift FFI round trip: append → candidates → context →
//! shrink → clear, including inputs that previously crashed the server
//! (interior NUL bytes, empty strings).

use shared::proto::azookey_service_client::AzookeyServiceClient;

async fn connect() -> AzookeyServiceClient<tonic::transport::Channel> {
    let channel = shared::pipe::lazy_pipe_channel(shared::pipe::SERVER_PIPE)
        .expect("failed to build pipe channel");
    let mut client = AzookeyServiceClient::new(channel);

    // readiness probe: the channel is lazy, so surface "server not running"
    // here with a clear message instead of on the first real assertion
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.clear_text(shared::proto::ClearTextRequest {}),
    )
    .await
    .expect("timed out connecting; is the server running? (see file header)")
    .expect("server is not running; start it first (see file header)");

    client
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn append_and_clear_roundtrip() {
    let mut client = connect().await;

    // start from a clean slate
    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");

    // the TSF client sends alphabet keys as halfwidth ASCII (roman input)
    let response = client
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: "k".to_string(),
        })
        .await
        .expect("append_text failed")
        .into_inner();
    let composing = response.composing_text.expect("composing_text missing");
    assert!(
        !composing.hiragana.is_empty(),
        "hiragana should not be empty"
    );

    let response = client
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: "a".to_string(),
        })
        .await
        .expect("append_text failed")
        .into_inner();
    let composing = response.composing_text.expect("composing_text missing");
    assert_eq!(composing.hiragana, "か");
    assert!(
        !composing.suggestions.is_empty(),
        "some candidate should be returned for か"
    );

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn hostile_inputs_do_not_kill_the_server() {
    let mut client = connect().await;

    // interior NUL byte: previously panicked in CString::new
    client
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: "a\0b".to_string(),
        })
        .await
        .expect("append_text with NUL failed");

    // NUL byte in context, plus \r-splitting edge cases
    for context in ["ctx\0evil", "", "\r\r\r", "前文\rコンテキスト"] {
        client
            .set_context(shared::proto::SetContextRequest {
                context: context.to_string(),
            })
            .await
            .expect("set_context failed");
    }

    // empty append (used by the client as a connection warm-up)
    client
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: String::new(),
        })
        .await
        .expect("empty append_text failed");

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");

    // the server must still answer after all of the above
    let response = client
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: "a".to_string(),
        })
        .await
        .expect("server no longer responds")
        .into_inner();
    assert_eq!(
        response
            .composing_text
            .expect("composing_text missing")
            .hiragana,
        "あ"
    );

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");
}
