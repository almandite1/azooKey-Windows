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

use shared::proto::{ComposingText, azookey_service_client::AzookeyServiceClient};

type Client = AzookeyServiceClient<tonic::transport::Channel>;

async fn connect() -> Client {
    let channel = shared::pipe::lazy_pipe_channel(shared::pipe::server_pipe())
        .expect("failed to build pipe channel");
    let mut client = AzookeyServiceClient::new(channel);

    // readiness probe: the channel is lazy, so surface "server not running"
    // here with a clear message instead of on the first real assertion
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.clear_text(shared::proto::ClearTextRequest {
            // no index: these calls end a composition without confirming
            // one, which is what the client sends on Escape — and what the
            // engine must learn nothing from
            candidate_index: None,
        }),
    )
    .await
    .expect("timed out connecting; is the server running? (see file header)")
    .expect("server is not running; start it first (see file header)");

    client
}

// One helper per RPC, because every call site was the same four-line chain:
// build the request, await it, `expect` the transport result, `into_inner`,
// and — for the calls that answer with one — `expect` the composing text out
// of its Option. `expect` throughout is right here: this is a smoke test
// against a live server, so a failed call IS the result.

async fn clear(client: &mut Client) {
    client
        .clear_text(shared::proto::ClearTextRequest {
            // no index: these calls end a composition without confirming
            // one, which is what the client sends on Escape — and what the
            // engine must learn nothing from
            candidate_index: None,
        })
        .await
        .expect("clear_text failed");
}

/// One keystroke, and the reading the engine answered with.
async fn append(client: &mut Client, text: &str) -> ComposingText {
    client
        .append_text(shared::proto::AppendTextRequest {
            text_to_append: text.to_string(),
        })
        .await
        .expect("append_text failed")
        .into_inner()
        .composing_text
        .expect("composing_text missing")
}

/// Every character of `keys` as its own keystroke — the way the TIP sends
/// them — and the composing text after the last one.
async fn type_keys(client: &mut Client, keys: &str) -> ComposingText {
    let mut composing = None;
    for key in keys.chars() {
        composing = Some(append(client, &key.to_string()).await);
    }
    composing.expect("no keystrokes were sent")
}

async fn remove(client: &mut Client) -> ComposingText {
    remove_many(client, 1).await
}

/// One RemoveText standing for `count` presses of a held Backspace.
async fn remove_many(client: &mut Client, count: i32) -> ComposingText {
    client
        .remove_text(shared::proto::RemoveTextRequest { count })
        .await
        .expect("remove_text failed")
        .into_inner()
        .composing_text
        .expect("composing_text missing")
}

async fn shrink(client: &mut Client, surface_offset: i32) {
    client
        .shrink_text(shared::proto::ShrinkTextRequest {
            surface_offset,
            candidate_index: None,
        })
        .await
        .expect("shrink_text failed (server crash?)");
}

async fn shrink_for(client: &mut Client, surface_offset: i32) -> ComposingText {
    client
        .shrink_text(shared::proto::ShrinkTextRequest {
            surface_offset,
            candidate_index: None,
        })
        .await
        .expect("shrink_text failed")
        .into_inner()
        .composing_text
        .expect("composing_text missing")
}

async fn set_context(client: &mut Client, context: &str) {
    client
        .set_context(shared::proto::SetContextRequest {
            context: context.to_string(),
        })
        .await
        .expect("set_context failed");
}

async fn move_cursor(client: &mut Client, offset: i32) -> ComposingText {
    client
        .move_cursor(shared::proto::MoveCursorRequest { offset })
        .await
        .expect("move_cursor failed")
        .into_inner()
        .composing_text
        .expect("composing_text missing")
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn append_and_clear_roundtrip() {
    let mut client = connect().await;

    // start from a clean slate
    clear(&mut client).await;

    // the TSF client sends alphabet keys as halfwidth ASCII (roman input)
    let composing = append(&mut client, "k").await;
    assert!(
        !composing.hiragana.is_empty(),
        "hiragana should not be empty"
    );

    let composing = append(&mut client, "a").await;
    assert_eq!(composing.hiragana, "か");
    assert!(
        !composing.suggestions.is_empty(),
        "some candidate should be returned for か"
    );

    clear(&mut client).await;
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
        append(client, key).await;
    }

    let a_final = append(&mut a, "i").await;

    // before per-session state, B's keystrokes would have been spliced
    // into A's composition (and vice versa)
    assert_eq!(a_final.hiragana, "かき");

    // B's composition is intact as well: append nothing new, just clear
    // after checking via one more keystroke round trip
    let b_final = remove(&mut b).await;
    // "すし" minus one deletion = "す"
    assert_eq!(b_final.hiragana, "す");

    for client in [&mut a, &mut b] {
        clear(client).await;
    }
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn long_composition_shrink_does_not_kill_the_server() {
    let mut client = connect().await;

    clear(&mut client).await;

    // 130 roman keystrokes (65 x "ka") = 65 kana. Two counts that must not
    // kill the engine follow: 130, which is past 127 (a former `as i8`
    // truncation wrapped such counts negative, and a negative count traps
    // the Swift engine outright) and is also past the end of this reading
    // (an unclamped surface count leaves the cursor negative and traps on
    // the NEXT keystroke instead), then -1.
    for _ in 0..65 {
        type_keys(&mut client, "ka").await;
    }

    shrink(&mut client, 130).await;
    // a hostile negative offset must be clamped, not trap the engine
    shrink(&mut client, -1).await;

    // the server must still answer
    append(&mut client, "a").await;

    clear(&mut client).await;
}

/// Unlike its neighbours this one also needs `plugin-host.exe` running and
/// `plugins.enable` true in settings.json — the date candidate is no
/// longer produced by the server. Which makes it the end-to-end proof of
/// the whole path: engine list -> pipe -> host -> pipe -> validation ->
/// placement. With the host stopped every other test here still passes and
/// this one simply finds no date, which is what fail-open is supposed to
/// look like from outside.
#[tokio::test]
#[ignore = "requires a running azookey-server AND plugin-host, with plugins.enable"]
async fn a_day_reading_gains_the_calendar_date() {
    // The candidate pipeline's unit tests pin what the stage does with a
    // list; only a live engine can show it running on the list the engine
    // actually produces — and, more to the point, that the span the added
    // candidate claims matches what the engine reported for the same
    // reading. `kyou` is four keystrokes and three kana; a date candidate
    // that disagreed would desync the client's raw input on commit.
    let mut client = connect().await;
    clear(&mut client).await;

    let composing = type_keys(&mut client, "kyou").await;
    assert_eq!(composing.hiragana, "きょう");

    let today = chrono::Local::now().date_naive();
    let expected = format!(
        "{:04}/{:02}/{:02}",
        chrono::Datelike::year(&today),
        chrono::Datelike::month(&today),
        chrono::Datelike::day(&today)
    );
    let dated = composing
        .suggestions
        .iter()
        .find(|s| s.text == expected)
        .unwrap_or_else(|| {
            let candidates: Vec<&str> = composing
                .suggestions
                .iter()
                .map(|s| s.text.as_str())
                .collect();
            panic!("{expected} should be among the candidates for きょう, got {candidates:?}")
        });

    assert_eq!(dated.surface_count, 3, "きょう is three kana");
    assert_eq!(dated.corresponding_count, 4, "kyou is four keystrokes");
    assert_eq!(dated.subtext, "日付");

    clear(&mut client).await;
}

/// Borrows the real `settings.json` for the length of a test and puts it back.
///
/// These are the only tests here that write to the machine's configuration,
/// and they have to: `UpdateConfig` is defined by what the file says, and
/// there is no seam for it on the far side of a live server. The original
/// bytes are restored on drop, including when a test panics — but a test
/// process KILLED mid-run leaves whatever it wrote, so if that happens,
/// restart the IME and it will move the file aside and start from defaults.
struct BorrowedSettings {
    path: std::path::PathBuf,
    original: Option<String>,
}

impl BorrowedSettings {
    fn take() -> Self {
        let path = shared::config_root()
            .expect("APPDATA must be set to run this test")
            .join("settings.json");
        let original = std::fs::read_to_string(&path).ok();
        BorrowedSettings { path, original }
    }

    fn write(&self, contents: &str) {
        std::fs::write(&self.path, contents).expect("write the settings fixture");
    }
}

impl Drop for BorrowedSettings {
    fn drop(&mut self) {
        match &self.original {
            Some(original) => {
                let _ = std::fs::write(&self.path, original);
            }
            // there was no file before us; leave none behind
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

async fn update_config(client: &mut Client) -> Result<(), tonic::Status> {
    client
        .update_config(shared::proto::UpdateConfigRequest {})
        .await
        .map(|_| ())
}

/// A settings file the server cannot read has to come back as an error.
///
/// It used to come back as success: `LoadConfig` was void, the Swift side kept
/// its old values on a decode failure, and the RPC returned `Ok` unconditionally
/// — so the settings app showed a success toast while conversion carried on
/// with the configuration it already had. There was no way to observe that from
/// outside the engine, which is why it needs a live one.
#[tokio::test]
#[ignore = "requires a running azookey-server; rewrites settings.json and restores it"]
async fn update_config_reports_a_settings_file_it_cannot_read() {
    let mut client = connect().await;
    let settings = BorrowedSettings::take();

    // valid JSON first: this must succeed, so that a failure below is about
    // the broken file and not about the RPC being broken generally
    settings.write(r#"{"version":"0.1.0","zenzai":{"enable":false},"plugins":{"enable":false}}"#);
    update_config(&mut client)
        .await
        .expect("a readable settings file must reload cleanly");

    settings.write("{this is not json");
    let error = update_config(&mut client)
        .await
        .expect_err("a settings file that cannot be read must not report success");
    assert!(
        !error.message().is_empty(),
        "the failure has to say something the settings app can show"
    );
}

/// `plugins.enable` has to take effect on the next keystroke, not the next
/// restart — and the engine's own reload does not carry it, so this is the
/// half that only the Rust side does.
///
/// Deterministic because the date plugin's candidate either is or is not in
/// the list; the zenzai keys change conversion in ways that are real but not
/// assertable.
#[tokio::test]
#[ignore = "requires a running azookey-server AND plugin-host; rewrites settings.json and restores it"]
async fn update_config_switches_the_plugin_hook_without_a_restart() {
    let mut client = connect().await;
    let settings = BorrowedSettings::take();

    let today = chrono::Local::now().date_naive();
    let expected = format!(
        "{:04}/{:02}/{:02}",
        chrono::Datelike::year(&today),
        chrono::Datelike::month(&today),
        chrono::Datelike::day(&today)
    );
    let date_is_offered =
        |composing: &ComposingText| composing.suggestions.iter().any(|s| s.text == expected);

    settings.write(r#"{"version":"0.1.0","zenzai":{"enable":false},"plugins":{"enable":true}}"#);
    update_config(&mut client).await.expect("enable plugins");
    clear(&mut client).await;
    let on = type_keys(&mut client, "kyou").await;
    clear(&mut client).await;
    assert!(
        date_is_offered(&on),
        "with plugins on, {expected} should be offered for きょう; is plugin-host running?"
    );

    settings.write(r#"{"version":"0.1.0","zenzai":{"enable":false},"plugins":{"enable":false}}"#);
    update_config(&mut client).await.expect("disable plugins");
    clear(&mut client).await;
    let off = type_keys(&mut client, "kyou").await;
    clear(&mut client).await;
    assert!(
        !date_is_offered(&off),
        "with plugins off the date must be gone without restarting the server"
    );
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
    clear(&mut client).await;

    let composing = type_keys(&mut client, "mizu").await;

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

    clear(&mut client).await;
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn committing_a_clause_leaves_the_rest_of_the_reading() {
    // The half of the shrink contract only a live engine can prove: the
    // count a candidate reports must, spent on the engine's own composition,
    // leave exactly the reading the candidate advertised as its subtext.
    //
    // へんかんする is the case that broke. The engine offers 「変換」for the
    // first four kana, but ん-す-る is one romaji cluster, so the boundary
    // has no keystroke count — sending one left うる composing while the
    // candidate window promised する.
    let mut client = connect().await;
    clear(&mut client).await;

    let composing = type_keys(&mut client, "henkansuru").await;
    assert_eq!(composing.hiragana, "へんかんする");

    let clause = composing
        .suggestions
        .iter()
        .find(|s| s.text == "変換")
        .expect("変換 should be among the candidates for へんかんする");
    assert_eq!(
        clause.subtext, "する",
        "the candidate window is told 変換 leaves する"
    );

    let after = shrink_for(&mut client, clause.surface_count).await;

    assert_eq!(
        after.hiragana, "する",
        "committing 変換 must leave exactly the reading it promised"
    );

    clear(&mut client).await;
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment; it LEARNS \
            from a conversion and then resets the learning history"]
async fn a_confirmed_candidate_is_learned_and_the_history_can_be_reset() {
    // The learning path end to end, which only a live engine can show: a
    // displayed index is translated to the engine's own, the engine learns
    // from the candidate behind it, and the whole history can be dropped
    // again.
    //
    // Deliberately destructive of the learning history and nothing else —
    // hence the second half. Run it on a development machine, not on one
    // whose learned conversions you want to keep.
    let mut client = connect().await;
    clear(&mut client).await;

    let composing = type_keys(&mut client, "mizu").await;
    assert_eq!(composing.hiragana, "みず");
    // whatever the engine ranks second, so the confirmation is a real
    // choice rather than the ranking it already had
    let chosen = i32::try_from(composing.suggestions.len().min(2) - 1).expect("a small index");

    client
        .clear_text(shared::proto::ClearTextRequest {
            candidate_index: Some(chosen),
        })
        .await
        .expect("a confirming clear_text must be accepted");

    client
        .reset_learning(shared::proto::ResetLearningRequest {})
        .await
        .expect("reset_learning failed; is there a memory directory?");

    // An index no list ever had must be refused by the translation rather
    // than reaching the engine — the pipe is open to any local process.
    let _ = type_keys(&mut client, "mizu").await;
    client
        .clear_text(shared::proto::ClearTextRequest {
            candidate_index: Some(9999),
        })
        .await
        .expect("an out-of-range index must be ignored, not fatal");
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
        assert_eq!(append(&mut first, "a").await.hiragana, "あ");
        // dropping `first` closes its pipe connection = the client "went away"
    }

    // a brand-new connection (new pipe, new session) must still be served
    let mut second = connect().await;
    assert_eq!(
        append(&mut second, "a").await.hiragana,
        "あ",
        "the server did not accept a reconnection after the first client dropped"
    );

    clear(&mut second).await;
}

#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn hostile_inputs_do_not_kill_the_server() {
    let mut client = connect().await;

    // interior NUL byte: previously panicked in CString::new
    append(&mut client, "a\0b").await;

    // NUL byte in context, plus \r-splitting edge cases
    for context in ["ctx\0evil", "", "\r\r\r", "前文\rコンテキスト"] {
        set_context(&mut client, context).await;
    }

    // empty append (used by the client as a connection warm-up)
    append(&mut client, "").await;

    clear(&mut client).await;

    // the server must still answer after all of the above
    assert_eq!(append(&mut client, "a").await.hiragana, "あ");

    clear(&mut client).await;
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

    clear(&mut client).await;
    type_keys(&mut client, "mizu").await;

    // moving the cursor must not change the composed text
    assert_eq!(move_cursor(&mut client, -1).await.hiragana, "みず");
    assert_eq!(move_cursor(&mut client, 1).await.hiragana, "みず");

    clear(&mut client).await;
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

    clear(&mut client).await;

    let reading = type_keys(&mut client, "saigentejun").await.hiragana;
    // The engine holds the trailing n unresolved — a follow-up key can
    // still make it な行 — but reports it as ん: this reading is what F6
    // shows and what a commit writes, so it must not carry a latin letter
    // (issue #38). Either way it is one character, which is what the
    // per-kana contract below actually depends on.
    assert_eq!(reading, "さいげんてじゅん");

    let mut expected = reading.chars().count();
    while expected > 0 {
        let hiragana = remove(&mut client).await.hiragana;
        expected -= 1;
        assert_eq!(
            hiragana.chars().count(),
            expected,
            "each RemoveText must delete exactly one kana (got {hiragana:?})"
        );
    }

    clear(&mut client).await;
}

/// A held Backspace arrives as ONE keydown standing for several presses, and
/// the count is what spares the engine a full reconversion per kana. The
/// contract the client relies on: `count` kana go, and the answer is the
/// conversion of what is left — indistinguishable from `count` separate calls,
/// which is what makes the batching safe.
#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn remove_text_deletes_a_whole_batch_of_kana_at_once() {
    let mut client = connect().await;

    clear(&mut client).await;

    let reading = type_keys(&mut client, "saigentejun").await.hiragana;
    assert_eq!(reading, "さいげんてじゅん");

    assert_eq!(
        remove_many(&mut client, 2).await.hiragana,
        "さいげんてじ",
        "count=2 must delete two kana in one call"
    );
    assert_eq!(
        remove_many(&mut client, 3).await.hiragana,
        "さいげ",
        "and the reading must keep shrinking by exactly the count"
    );

    // A count past the end empties the reading rather than failing: the
    // client clamps to the reading it can see, but the two sides can disagree
    // for one keystroke (the engine holds a trailing ん unresolved), and a
    // panic here would take the whole server down.
    assert_eq!(remove_many(&mut client, 99).await.hiragana, "");

    // ...and the session still works afterwards
    assert_eq!(type_keys(&mut client, "a").await.hiragana, "あ");

    clear(&mut client).await;
}

/// Zero and negative counts are what a TIP older than the count field sends
/// (it sends nothing, which decodes to 0). Deleting nothing would strand the
/// old client in a composition it cannot back out of, so the server clamps up
/// to the one-kana behaviour that client was built for.
#[tokio::test]
#[ignore = "requires a running azookey-server with its DLL environment"]
async fn a_count_below_one_still_deletes_one_kana() {
    let mut client = connect().await;

    clear(&mut client).await;
    assert_eq!(type_keys(&mut client, "mizu").await.hiragana, "みず");

    assert_eq!(remove_many(&mut client, 0).await.hiragana, "み");
    assert_eq!(remove_many(&mut client, -5).await.hiragana, "");

    clear(&mut client).await;
}
