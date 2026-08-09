//! The gRPC service implementation: request/response plumbing only.
//! Engine access goes through the safe wrappers in wrappers.rs.

use std::collections::HashMap;
use std::sync::Mutex;

use tonic::{Request, Response, Status};

use shared::proto::azookey_service_server::AzookeyService;
use shared::proto::{
    AppendTextRequest, AppendTextResponse, ClearTextRequest, ClearTextResponse, ComposingText,
    MoveCursorRequest, MoveCursorResponse, RemoveTextRequest, RemoveTextResponse,
    ShrinkTextRequest, ShrinkTextResponse, Suggestion,
};

use crate::candidate_pipeline::{self, StageInput};
use crate::memory_dir::ensure_secured_memory_dir;
use crate::plugin_client::PluginClient;
use crate::session::session_of;
use crate::wrappers::{
    RawComposingText, add_text, clear_text, get_composed_text, load_config, move_cursor,
    remove_text, reset_learning, set_context, shrink_text,
};

pub struct MyAzookeyService {
    plugins: PluginClient,
    /// For each session, how the candidate list the window is showing maps
    /// back onto the engine's own.
    ///
    /// The client can only name a row it can see, and the engine can only
    /// learn from a candidate it produced — and the two lists are not the
    /// same list: the pipeline drops duplicates and lets plugins insert
    /// rows. This is the translation, rebuilt on every conversion.
    ///
    /// A `Mutex` because the trait's methods take `&self`. It is never held
    /// across an await, and the runtime is single-threaded anyway, so it is
    /// bookkeeping rather than concurrency.
    ///
    /// ACCEPTED: an entry outlives its session when a client dies
    /// mid-composition. `clear_text` drops it on every ordinary end —
    /// confirm, Escape, focus loss, a host-terminated composition — so what
    /// is left is one small vector per connection that never got to finish,
    /// and a session id is never reused. Wiring it to the idle eviction in
    /// session.rs would mean threading this map through a free function
    /// every handler calls, which costs more than the leak.
    displayed_engine_indices: Mutex<HashMap<i64, Vec<Option<i32>>>>,
}

/// Where each displayed candidate sits in the engine's own list.
///
/// Matched on text, which is exact rather than approximate here: the
/// pipeline's dedup keeps the FIRST candidate carrying a given text, so
/// `position` finds precisely the row that survived; and a plugin may not
/// add a text the engine already produced, so a plugin row never matches
/// one — it maps to `None`, and learns nothing.
///
/// O(displayed × engine) and deliberately so: both lists are a candidate
/// window's worth, and an index built per keystroke would cost more than the
/// scan it saves.
fn engine_indices(engine: &[Suggestion], displayed: &[Suggestion]) -> Vec<Option<i32>> {
    displayed
        .iter()
        .map(|shown| {
            engine
                .iter()
                .position(|candidate| candidate.text == shown.text)
                .and_then(|index| i32::try_from(index).ok())
        })
        .collect()
}

/// Ceiling on one `RemoveText` batch.
///
/// The client already clamps the count to the reading it can see, so a batch
/// this large means the two sides disagree — a stale TIP, or a repeat count a
/// host inflated. Deleting from an empty reading is harmless on the engine
/// side, so the cap is not about correctness; it bounds how long one FFI call
/// can hold the single-threaded runtime, which is what a health ping and every
/// other application's keystrokes are waiting on.
const MAX_REMOVE_TEXT_COUNT: i32 = 512;

impl MyAzookeyService {
    pub fn new() -> Self {
        MyAzookeyService {
            plugins: PluginClient::new(),
            displayed_engine_indices: Mutex::new(HashMap::new()),
        }
    }

    /// Turns the index the client confirmed — a row in the list its window
    /// was showing — into an index the engine can learn from, or -1.
    ///
    /// -1 for every way this can fail to name an engine candidate: no
    /// mapping for the session (nothing was ever converted), a row that came
    /// from a plugin, or an index outside the list. Out of range is warned
    /// about and then treated like the rest: this arrives over a pipe any
    /// local process can open, so it is untrusted input, not an assertion.
    fn engine_index(&self, session: i64, displayed: Option<i32>) -> i32 {
        let Some(displayed) = displayed else {
            return -1;
        };
        let Ok(map) = self.displayed_engine_indices.lock() else {
            return -1;
        };
        let Some(indices) = map.get(&session) else {
            return -1;
        };
        let row = usize::try_from(displayed)
            .ok()
            .and_then(|index| indices.get(index).copied());
        match row {
            // a row we sent, and the engine's own index for it
            Some(Some(engine)) => engine,
            // a row we sent that the engine did not produce: a plugin's.
            // Ordinary, so no warning.
            Some(None) => -1,
            None => {
                tracing::warn!(
                    displayed,
                    shown = indices.len(),
                    "the confirmed candidate is not a row of the list we last \
                     sent; learning nothing from it"
                );
                -1
            }
        }
    }

    /// Builds the ComposingText payload every composing-text RPC returns:
    /// the current hiragana plus a fresh candidate fetch for the session,
    /// run through the candidate pipeline. Every RPC that returns
    /// candidates goes through here, so the pipeline hook lives in this
    /// one place.
    ///
    /// The plugin host is asked BEFORE the pipeline runs and its answer is
    /// carried in as data, which is what keeps the pipeline a synchronous
    /// pure function. Awaiting here does not stall the single-threaded
    /// runtime — only the FFI does that — and with plugins off (the
    /// default) there is nothing to await at all.
    async fn composed(&self, session: i64, composing_text: RawComposingText) -> ComposingText {
        let hiragana = composing_text.text;
        let candidates = get_composed_text(session);
        let offered = self.plugins.offer(&hiragana, &candidates).await;
        // The host and the validator are given the same view of what the
        // engine produced: this list, before the pipeline removes
        // anything from it.
        let input = StageInput::new(&hiragana, &offered, &candidates);
        // Kept because the pipeline consumes the list and this is the one
        // place both orders exist at once: the client will eventually
        // confirm a row of `suggestions`, and the engine can only learn from
        // a row of what it produced.
        let engine = candidates.clone();
        let suggestions = candidate_pipeline::run(&input, candidates);
        if let Ok(mut map) = self.displayed_engine_indices.lock() {
            map.insert(session, engine_indices(&engine, &suggestions));
        }
        ComposingText {
            hiragana,
            suggestions,
        }
    }
}

/// The engine wants the text immediately left of the caret on the current
/// line as conversion context. Take the last non-empty line, splitting on BOTH
/// `\r` and `\n`: splitting on `\r` alone left a leading `\n` on the segment in
/// CRLF documents, feeding a stray newline into the engine's left-side context.
fn last_line_context(context: &str) -> &str {
    context
        .split(['\r', '\n'])
        .rfind(|s| !s.is_empty())
        .unwrap_or_default()
}

#[tonic::async_trait]
impl AzookeyService for MyAzookeyService {
    async fn append_text(
        &self,
        request: Request<AppendTextRequest>,
    ) -> Result<Response<AppendTextResponse>, Status> {
        let session = session_of(&request);
        let input = request.into_inner().text_to_append;

        Ok(Response::new(AppendTextResponse {
            composing_text: Some(self.composed(session, add_text(session, &input)).await),
        }))
    }

    async fn remove_text(
        &self,
        request: Request<RemoveTextRequest>,
    ) -> Result<Response<RemoveTextResponse>, Status> {
        let session = session_of(&request);
        let count = request.into_inner().count.clamp(1, MAX_REMOVE_TEXT_COUNT);

        // The loop is the cheap half: RemoveText drops one kana from the
        // reading and does no conversion. `composed` is the expensive one — it
        // reconverts the whole reading, Zenzai inference included — so it runs
        // once for the batch rather than once per kana. That is the entire
        // point of the count.
        let mut composing_text = remove_text(session);
        for _ in 1..count {
            composing_text = remove_text(session);
        }

        Ok(Response::new(RemoveTextResponse {
            composing_text: Some(self.composed(session, composing_text).await),
        }))
    }

    async fn move_cursor(
        &self,
        request: Request<MoveCursorRequest>,
    ) -> Result<Response<MoveCursorResponse>, Status> {
        let session = session_of(&request);
        let offset = request.into_inner().offset;

        Ok(Response::new(MoveCursorResponse {
            composing_text: Some(self.composed(session, move_cursor(session, offset)).await),
        }))
    }

    async fn clear_text(
        &self,
        request: Request<ClearTextRequest>,
    ) -> Result<Response<ClearTextResponse>, Status> {
        let session = session_of(&request);
        let confirmed = request.into_inner().candidate_index;
        // Absent means the composition was thrown away rather than
        // confirmed — Escape, focus loss, a host that terminated it — or a
        // TIP from before the field existed. All of those learn nothing.
        let engine_index = self.engine_index(session, confirmed);
        clear_text(session, engine_index);
        // The composition is over, so the mapping has nothing left to
        // translate. Dropped here rather than aged out: this is the one
        // moment we know for certain it is spent.
        if let Ok(mut map) = self.displayed_engine_indices.lock() {
            map.remove(&session);
        }
        Ok(Response::new(ClearTextResponse {}))
    }

    async fn shrink_text(
        &self,
        request: Request<ShrinkTextRequest>,
    ) -> Result<Response<ShrinkTextResponse>, Status> {
        let session = session_of(&request);
        let request = request.into_inner();
        let surface_offset = request.surface_offset;
        // Translated NOW, before `composed` below replaces the mapping with
        // the one for the remainder of the reading.
        let engine_index = self.engine_index(session, request.candidate_index);

        // Committing a candidate that spends no kana is something the client
        // never has reason to ask for — the candidate it just confirmed always
        // covers at least one kana of the reading. In practice a zero means a
        // TIP older than the switch of this count from keystrokes to kana: it
        // sends its number in proto field 1, retired when `surface_offset`
        // was renumbered to 2, so nothing arrives here. The reading then
        // survives the commit and the next preview re-converts the whole thing
        // — on screen the confirmed clause looks duplicated (せいちょうせんりゃく
        // committed as 政庁 came back as 政庁成長戦略). The skew is silent
        // otherwise, because an application that was already running keeps the
        // DLL it loaded even after the new one is registered. A warning, not an
        // error: the old TIP is still usable, just wrong at this one boundary.
        if surface_offset <= 0 {
            tracing::warn!(
                surface_offset,
                "ShrinkText spends no kana, so the reading survives the commit. \
                 This is what a TIP built before the kana-unit shrink looks \
                 like: re-register the current DLL and restart the application \
                 that is typing."
            );
        }

        Ok(Response::new(ShrinkTextResponse {
            composing_text: Some(
                self.composed(session, shrink_text(session, surface_offset, engine_index))
                    .await,
            ),
        }))
    }

    async fn set_context(
        &self,
        request: Request<shared::proto::SetContextRequest>,
    ) -> Result<Response<shared::proto::SetContextResponse>, Status> {
        let session = session_of(&request);
        let context = request.into_inner().context;

        set_context(session, last_line_context(&context));
        Ok(Response::new(shared::proto::SetContextResponse {}))
    }

    async fn update_config(
        &self,
        _: Request<shared::proto::UpdateConfigRequest>,
    ) -> Result<Response<shared::proto::UpdateConfigResponse>, Status> {
        // Before the read, because the document about to be built names the
        // learning directory and the engine adopts it only if it EXISTS. It
        // also heals a directory the user deleted by hand, and re-applies
        // the ACL if something replaced it.
        ensure_secured_memory_dir();
        // ONE read, here, handed to both halves. The engine used to open
        // settings.json for itself, so this RPC read the file twice and a save
        // landing between the two reads applied half of each version.
        let config = shared::AppConfig::try_read().map_err(Status::failed_precondition)?;
        let json = config
            .to_engine_json()
            .map_err(|e| Status::internal(format!("could not pass the settings on: {e}")))?;

        // Reported, not swallowed. A decode failure in the engine used to stay
        // there: the settings app showed a success toast while conversion
        // carried on with the values it already had.
        if !load_config(&json) {
            return Err(Status::internal(
                "the conversion engine refused the new settings and kept the ones it had",
            ));
        }
        // the same signal reaches the Rust side: `plugins.enable` lives in
        // settings.json too, and the engine's reload does not carry it
        self.plugins.reload_config();
        Ok(Response::new(shared::proto::UpdateConfigResponse {}))
    }

    async fn reset_learning(
        &self,
        _: Request<shared::proto::ResetLearningRequest>,
    ) -> Result<Response<shared::proto::ResetLearningResponse>, Status> {
        // The engine refuses to reset what it cannot see, and it never
        // creates the directory itself — so make sure there is one, with our
        // permissions on it, before asking.
        ensure_secured_memory_dir();
        if !reset_learning() {
            return Err(Status::failed_precondition(
                "the conversion engine has no learning history to reset",
            ));
        }
        Ok(Response::new(shared::proto::ResetLearningResponse {}))
    }
}

#[cfg(test)]
mod tests {
    //! Pure-function tests: nothing here may reference an FFI symbol (see
    //! the warning in wrappers.rs — one import would make every test in the
    //! crate need the Swift runtime).

    use super::{engine_indices, last_line_context};
    use shared::proto::Suggestion;

    fn suggestions(texts: &[&str]) -> Vec<Suggestion> {
        texts
            .iter()
            .map(|text| Suggestion {
                text: (*text).to_string(),
                subtext: String::new(),
                corresponding_count: 1,
                surface_count: 1,
            })
            .collect()
    }

    #[test]
    fn an_untouched_list_maps_onto_itself() {
        let engine = suggestions(&["今日", "きょう", "強"]);

        assert_eq!(
            engine_indices(&engine, &engine),
            vec![Some(0), Some(1), Some(2)]
        );
    }

    /// Dedup keeps the FIRST candidate carrying a text, so matching by text
    /// finds exactly the row that survived — not merely one that looks like
    /// it.
    #[test]
    fn a_deduplicated_list_points_at_the_surviving_row() {
        let engine = suggestions(&["今日", "強", "今日", "京"]);
        let displayed = suggestions(&["今日", "強", "京"]);

        assert_eq!(
            engine_indices(&engine, &displayed),
            vec![Some(0), Some(1), Some(3)]
        );
    }

    /// A plugin may not offer a text the engine already produced, so its row
    /// never matches one — and there is nothing for the engine to learn from
    /// a candidate it did not make.
    #[test]
    fn a_plugin_row_maps_to_nothing() {
        let engine = suggestions(&["今日", "強", "京"]);
        let displayed = suggestions(&["今日", "強", "2026年8月7日", "京"]);

        assert_eq!(
            engine_indices(&engine, &displayed),
            vec![Some(0), Some(1), None, Some(2)]
        );
    }

    #[test]
    fn an_empty_list_maps_to_nothing_at_all() {
        assert!(engine_indices(&suggestions(&["今日"]), &[]).is_empty());
        assert_eq!(engine_indices(&[], &suggestions(&["今日"])), vec![None]);
    }

    /// `UpdateConfig` has to reload BOTH halves of the settings file, and
    /// nothing was checking that it still did.
    ///
    /// The engine gets the zenzai keys over the FFI; `plugins.enable` is read
    /// on this side, because the engine's reload does not carry it. Deleting
    /// either call left the whole suite green — the engine half needs a live
    /// engine to observe, and the plugin half needs a live host — while the
    /// symptom for a user was a setting that saved and then did nothing until
    /// they restarted.
    ///
    /// So the wiring is read instead, the same way `installer_config.rs` reads
    /// the installer's. It cannot tell whether the calls WORK; it can tell
    /// that both are still there, which is the failure that actually happened.
    #[test]
    fn update_config_reloads_the_engine_and_the_plugin_hook() {
        const SOURCE: &str = include_str!("service.rs");

        let start = SOURCE
            .find("async fn update_config")
            .expect("service.rs should implement update_config");
        let rest = &SOURCE[start..];
        // to the next method, or the end of the impl
        let end = rest[1..]
            .find("\n    async fn ")
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        assert!(
            body.contains("load_config(&json)"),
            "update_config must hand the settings to the engine: got {body}"
        );
        assert!(
            body.contains("self.plugins.reload_config()"),
            "update_config must also re-read plugins.enable, which the engine's \
             reload does not carry: got {body}"
        );
        assert!(
            body.contains("try_read"),
            "the document must be read ONCE here and passed on, rather than \
             read again on the far side of the FFI boundary: got {body}"
        );
        assert!(
            body.contains("ensure_secured_memory_dir()"),
            "the learning directory must exist, with our permissions on it, \
             before the document naming it reaches the engine — the engine \
             adopts the path only if the directory is already there, and it \
             must never create it itself: got {body}"
        );
    }

    #[test]
    fn last_line_context_takes_the_final_nonempty_line() {
        assert_eq!(last_line_context("前の行\r\n現在の行"), "現在の行");
        assert_eq!(last_line_context("前の行\n現在の行"), "現在の行");
        assert_eq!(last_line_context("前の行\r現在の行"), "現在の行");
        // no stray newline survives on a CRLF boundary
        assert_eq!(last_line_context("a\r\nb"), "b");
        // trailing newlines fall back to the previous non-empty line
        assert_eq!(last_line_context("最後\r\n"), "最後");
        // single line and empty input
        assert_eq!(last_line_context("ただ一行"), "ただ一行");
        assert_eq!(last_line_context(""), "");
        assert_eq!(last_line_context("\r\n\r\n"), "");
    }
}
