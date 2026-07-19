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
    let channel = shared::pipe::lazy_pipe_channel(shared::pipe::server_pipe())
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
async fn sessions_are_isolated_between_connections() {
    // each connect() opens its own pipe connection = its own session
    let mut a = connect().await;
    let mut b = connect().await;

    // interleave typing on both connections: "kaki" on A, "susi" on B
    for (on_a, key) in [
        (true, "k"),
        (false, "s"),
        (true, "a"),
        (false, "u"),
        (true, "k"),
        (false, "s"),
        (false, "i"),
    ] {
        let client = if on_a { &mut a } else { &mut b };
        client
            .append_text(shared::proto::AppendTextRequest {
                text_to_append: key.to_string(),
            })
            .await
            .expect("append_text failed");
    }

    let a_final = a
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: "i".to_string(),
        })
        .await
        .expect("append_text failed")
        .into_inner()
        .composing_text
        .expect("composing_text missing");

    // before per-session state, B's keystrokes would have been spliced
    // into A's composition (and vice versa)
    assert_eq!(a_final.hiragana, "かき");

    // B's composition is intact as well: append nothing new, just clear
    // after checking via one more keystroke round trip
    let b_final = b
        .remove_text(shared::proto::RemoveTextRequest {})
        .await
        .expect("remove_text failed")
        .into_inner()
        .composing_text
        .expect("composing_text missing");
    // "すし" minus one deletion = "す"
    assert_eq!(b_final.hiragana, "す");

    for client in [&mut a, &mut b] {
        client
            .clear_text(shared::proto::ClearTextRequest {})
            .await
            .expect("clear_text failed");
    }
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn long_composition_shrink_does_not_kill_the_server() {
    let mut client = connect().await;

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");

    // 130 roman keystrokes (65 x "ka"): the shrink offset is a count of
    // keystrokes, so a long composition pushes it past 127. A former
    // `as i8` truncation wrapped such counts negative, and the negative
    // count made the Swift engine trap (Array.removeFirst), killing the
    // whole server process.
    for _ in 0..65 {
        for key in ["k", "a"] {
            client
                .append_text(shared::proto::AppendTextRequest {
                    text_to_append: key.to_string(),
                })
                .await
                .expect("append_text failed");
        }
    }

    client
        .shrink_text(shared::proto::ShrinkTextRequest { offset: 130 })
        .await
        .expect("shrink_text with a >127 offset failed (server crash?)");

    // a hostile negative offset must be clamped, not trap the engine
    client
        .shrink_text(shared::proto::ShrinkTextRequest { offset: -1 })
        .await
        .expect("shrink_text with a negative offset failed (server crash?)");

    // the server must still answer
    let response = client
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: "a".to_string(),
        })
        .await
        .expect("server no longer responds")
        .into_inner();
    assert!(response.composing_text.is_some(), "composing_text missing");

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn conversion_yields_the_expected_candidate() {
    // The canonical end-to-end proof: the whole pipe -> gRPC -> Swift FFI ->
    // kana-kanji conversion path is alive and returns real candidates. This
    // is the "mizu -> 水" conversion the reactor-panic fix (70d367c) finally
    // made work in a real app; if any link in that chain regresses, the
    // roundtrip tests still pass but this one stops finding 水.
    let mut client = connect().await;
    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");

    let mut composing = None;
    for key in ["m", "i", "z", "u"] {
        composing = Some(
            client
                .append_text(shared::proto::AppendTextRequest {
                    text_to_append: key.to_string(),
                })
                .await
                .expect("append_text failed")
                .into_inner()
                .composing_text
                .expect("composing_text missing"),
        );
    }
    let composing = composing.expect("no keystrokes were sent");

    assert_eq!(composing.hiragana, "みず", "roman 'mizu' should read みず");
    let candidates: Vec<&str> = composing
        .suggestions
        .iter()
        .map(|s| s.text.as_str())
        .collect();
    assert!(
        candidates.contains(&"水"),
        "水 should be among the candidates for みず, got {candidates:?}"
    );

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn server_accepts_a_reconnection_after_a_client_drops() {
    // Recovery from the server's side: when a client process goes away, the
    // server must free that session and keep accepting new connections. This
    // is the automatable half of "the IME recovers when a peer restarts" —
    // the client-side lazy reconnect after an engine restart still needs the
    // manual checklist, since a test can't restart the server it depends on.
    {
        let mut first = connect().await;
        let response = first
            .append_text(shared::proto::AppendTextRequest {
                text_to_append: "a".to_string(),
            })
            .await
            .expect("append_text failed")
            .into_inner();
        assert_eq!(
            response
                .composing_text
                .expect("composing_text missing")
                .hiragana,
            "あ"
        );
        // dropping `first` closes its pipe connection = the client "went away"
    }

    // a brand-new connection (new pipe, new session) must still be served
    let mut second = connect().await;
    let response = second
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: "a".to_string(),
        })
        .await
        .expect("server did not accept a reconnection after the first client dropped")
        .into_inner();
    assert_eq!(
        response
            .composing_text
            .expect("composing_text missing")
            .hiragana,
        "あ"
    );

    second
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

/// MoveCursor is live end-to-end on the server side (proto → handler →
/// Swift engine) but the TSF client does not call it yet: its
/// ClientAction::MoveCursor interpreter arm is a deliberate no-op until
/// the predictive-conversion feature wires it up. This test keeps the
/// vertical slice from rotting in the meantime.
#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn move_cursor_round_trips_without_breaking_the_composition() {
    let mut client = connect().await;

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");

    for key in ["m", "i", "z", "u"] {
        client
            .append_text(shared::proto::AppendTextRequest {
                text_to_append: key.to_string(),
            })
            .await
            .expect("append_text failed");
    }

    // moving the cursor must not change the composed text
    let response = client
        .move_cursor(shared::proto::MoveCursorRequest { offset: -1 })
        .await
        .expect("move_cursor failed")
        .into_inner();
    let composing = response.composing_text.expect("composing_text missing");
    assert_eq!(composing.hiragana, "みず");

    let response = client
        .move_cursor(shared::proto::MoveCursorRequest { offset: 1 })
        .await
        .expect("move_cursor failed")
        .into_inner();
    let composing = response.composing_text.expect("composing_text missing");
    assert_eq!(composing.hiragana, "みず");

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");
}

/// The Backspace fix for upstream issue #35 relies on this engine
/// contract: one RemoveText call deletes exactly one kana from the
/// reading. The client predicts "this removal empties the composition"
/// from the READING length (raw_hiragana) — never from the converted
/// candidate's length, which can be shorter (さい → 際 is 1 char at 2
/// kana) and used to make the client commit the leftover reading.
#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn remove_text_drains_the_reading_one_kana_per_call() {
    let mut client = connect().await;

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");

    let mut reading = String::new();
    for key in "saigentejun".chars() {
        let response = client
            .append_text(shared::proto::AppendTextRequest {
                text_to_append: key.to_string(),
            })
            .await
            .expect("append_text failed")
            .into_inner();
        reading = response
            .composing_text
            .expect("composing_text missing")
            .hiragana;
    }
    // a lone trailing n stays roman until a follow-up key resolves it
    assert_eq!(reading, "さいげんてじゅn");

    let mut expected = reading.chars().count();
    while expected > 0 {
        let response = client
            .remove_text(shared::proto::RemoveTextRequest {})
            .await
            .expect("remove_text failed")
            .into_inner();
        let hiragana = response
            .composing_text
            .expect("composing_text missing")
            .hiragana;
        expected -= 1;
        assert_eq!(
            hiragana.chars().count(),
            expected,
            "each RemoveText must delete exactly one kana (got {hiragana:?})"
        );
    }

    client
        .clear_text(shared::proto::ClearTextRequest {})
        .await
        .expect("clear_text failed");
}
