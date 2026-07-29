//! The candidate post-processing pipeline: every RPC that returns a
//! candidate list flows through `run` (called from service.rs::composed,
//! the single merge point), so a new transformation is a new
//! `CandidateStage` variant plus its match arm here — no other file.
//!
//! Everything here is a pure function over (input, candidates). The one
//! stage whose material comes from outside the process — what the plugin
//! host offered — receives it as data in `StageInput`, fetched before the
//! pipeline runs. That is deliberate: it keeps the whole module
//! synchronous and testable with no pipe, no host, and no FFI, which
//! matters most for the part that decides what an outside process is
//! allowed to put in front of the user.

use shared::proto::Suggestion;

/// One transformation of the candidate list. The match in `apply` is
/// exhaustive on purpose: adding a variant without deciding what it does
/// is a compile error, not a silent no-op.
pub(crate) enum CandidateStage {
    /// Drops candidates whose surface text has already appeared, keeping
    /// the first (highest-ranked) occurrence. Subtext is deliberately not
    /// part of the key: two candidates that render the same are duplicates
    /// to the user regardless of how their annotations differ.
    Dedup,
    /// Checks what the plugin host offered and places what survives.
    PluginHook,
}

/// The fixed order every candidate list goes through.
///
/// Dedup runs FIRST, and that is load-bearing rather than tidy: a rank
/// means a position in the list the user actually sees, and the engine's
/// raw list repeats itself near the top. Inserting at index 4 before
/// deduplicating put an added candidate at index 2 on a live engine — the
/// rank was measured against rows that were about to be removed.
const STAGES: &[CandidateStage] = &[CandidateStage::Dedup, CandidateStage::PluginHook];

/// Everything a stage needs besides the candidate list.
pub(crate) struct StageInput<'a> {
    /// The hiragana the candidates convert.
    pub(crate) reading: &'a str,
    /// What the plugin host answered for this reading — UNVALIDATED, and
    /// treated as hostile input until `admit` says otherwise.
    ///
    /// Empty when plugins are switched off, when the host is unreachable,
    /// when it timed out, and when it answered nothing. Those are four
    /// different events to the client that fetched this and exactly one
    /// thing here: nothing to add. That is what fail-open means in this
    /// file.
    pub(crate) offered: &'a [Suggestion],
    /// Every (surface_count, corresponding_count) the ENGINE produced for
    /// this reading, collected before anything was removed from the list.
    ///
    /// Before dedup, deliberately. The host is shown the list as the
    /// engine ranked it, so it can copy a span from a row that dedup is
    /// about to drop — same text, different span. Corroborating against
    /// the deduplicated list would then refuse a span the engine really
    /// did price, and the public contract ("copy the counts from a
    /// candidate you were given") would be a promise the core does not
    /// keep. Fail-open makes that harmless and invisible, which is worse.
    priced_spans: std::collections::HashSet<(i32, i32)>,
}

impl<'a> StageInput<'a> {
    pub(crate) fn new(
        reading: &'a str,
        offered: &'a [Suggestion],
        engine: &[Suggestion],
    ) -> StageInput<'a> {
        StageInput {
            reading,
            offered,
            priced_spans: engine
                .iter()
                .map(|c| (c.surface_count, c.corresponding_count))
                .collect(),
        }
    }
}

/// Applies one stage.
fn apply(
    stage: &CandidateStage,
    input: &StageInput,
    candidates: Vec<Suggestion>,
) -> Vec<Suggestion> {
    match stage {
        CandidateStage::Dedup => dedup(candidates),
        CandidateStage::PluginHook => plugin_hook(input, candidates),
    }
}

/// Runs the full pipeline in the order `STAGES` fixes.
pub(crate) fn run(input: &StageInput, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    STAGES.iter().fold(candidates, |candidates, stage| {
        apply(stage, input, candidates)
    })
}

/// Where a candidate the pipeline did not convert goes in the list.
///
/// The candidate window shows five at a time and pages by five
/// (`MAX_VISIBLE_CANDIDATES` in crates/ui/assets/candidate.js), so index 4
/// is the last slot a user sees without paging. Appending instead put an
/// added candidate at position 331 of 334 for きょう — correct, and
/// unreachable.
///
/// Revisit this if the window's page size changes; the two numbers are
/// related by "the last visible slot", not by coincidence.
const ADDED_CANDIDATE_RANK: usize = 4;

/// How many added candidates the pipeline will place, in total, per list.
/// A bound on how far the engine's own ranking can be pushed down, and the
/// number a plugin must not be able to raise.
const MAX_ADDED_CANDIDATES: usize = 3;

/// Places candidates that no conversion produced.
///
/// THE POLICY, which plugins do not get a say in:
///
/// - index 0 is never displaced. It is what Enter commits, so moving it
///   would change what typing does — the one thing none of this may do.
///   With a non-empty list the clamp below cannot reach 0, and an EMPTY
///   list is refused outright: an addition would land at index 0 and
///   become what Enter commits, with nothing from the engine behind it.
///   Today `admit` already refuses everything when the engine produced
///   nothing (no span can be corroborated), so this rule changes no
///   behaviour — which is exactly why it is written here rather than left
///   as a side effect of a rule that lives somewhere else and could be
///   relaxed on its own terms.
/// - everything added goes at one fixed rank, decided here. A plugin that
///   could ask for a rank would compete with the next plugin for the top
///   of the list, and the user would arbitrate a fight they never asked to
///   have. This is why the wire format carries additions rather than a
///   replacement list: there is nothing for a plugin to say about order.
/// - at most `MAX_ADDED_CANDIDATES` survive, so the bound holds however
///   many plugins want in.
/// - a text the list already carries is not added again, and neither is
///   one the SAME answer already offered. The Dedup stage cannot clean up
///   after this one — it runs before it, and has to, for the rank to mean
///   a row the user will see — so duplicates among the additions would
///   reach the window. One host answering twice is easy to dismiss as a
///   host bug; the moment several plugins are summed into one answer it
///   stops being anybody's bug in particular.
///
/// The rank is clamped to the list, so a short list appends rather than
/// leaving a gap.
fn insert_added(mut candidates: Vec<Suggestion>, added: Vec<Suggestion>) -> Vec<Suggestion> {
    if candidates.is_empty() {
        return candidates;
    }

    let mut seen: std::collections::HashSet<String> =
        candidates.iter().map(|c| c.text.clone()).collect();
    let fresh: Vec<Suggestion> = added
        .into_iter()
        .filter(|c| seen.insert(c.text.clone()))
        .take(MAX_ADDED_CANDIDATES)
        .collect();

    let at = ADDED_CANDIDATE_RANK.min(candidates.len());
    candidates.splice(at..at, fresh);
    candidates
}

/// Why a candidate the plugin host offered was refused. Carried as a
/// reason rather than a bool so the log says which rule was broken —
/// there is no other way for a plugin author to find out.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    EmptyText,
    /// Covers no kana, or more kana than the reading has.
    SurfaceCountOutOfRange,
    NegativeCorrespondingCount,
    /// No candidate from the engine covers this same span, so nothing
    /// corroborates the keystroke count.
    SpanNotCorroborated,
}

/// Whether an offered candidate may be shown.
///
/// The counts are the dangerous part, and the reason is not obvious: they
/// are not decoration, they are what the commit spends. `surface_count` is
/// the kana ShrinkText removes from the reading, and `corresponding_count`
/// is what the client drops from its own keystroke buffer. A candidate
/// carrying the wrong pair does not look wrong in the window — it commits,
/// and leaves the reading or the raw input out of step with what is on
/// screen, which is the shape of the duplicated-clause bug this project
/// has already paid for once.
///
/// A plugin cannot compute `corresponding_count`: the same reading can be
/// typed kyou or kilyou and only the engine tracked which. So the rule is
/// not a range check but corroboration — some candidate the engine itself
/// produced must claim exactly this span. A plugin can only offer text for
/// a span the engine already priced.
fn admit(
    candidate: &Suggestion,
    reading: &str,
    priced_spans: &std::collections::HashSet<(i32, i32)>,
) -> Result<(), Refusal> {
    if candidate.text.is_empty() {
        return Err(Refusal::EmptyText);
    }
    let reading_kana = reading.chars().count() as i32;
    if candidate.surface_count < 1 || candidate.surface_count > reading_kana {
        return Err(Refusal::SurfaceCountOutOfRange);
    }
    if candidate.corresponding_count < 0 {
        return Err(Refusal::NegativeCorrespondingCount);
    }
    if !priced_spans.contains(&(candidate.surface_count, candidate.corresponding_count)) {
        return Err(Refusal::SpanNotCorroborated);
    }
    Ok(())
}

/// How many of the host's candidates are even looked at.
///
/// The host is sent at most `REQUEST_CANDIDATE_LIMIT` (16, in
/// plugin_client.rs) and may place at most `MAX_ADDED_CANDIDATES` (3), so
/// anything past this could not be shown regardless. Without the bound,
/// checking is where an oversized answer would cost: `admit` is a lookup
/// per candidate and this runs per keystroke, but — unlike the call
/// itself — it is NOT inside the timeout. A host answering with ten
/// thousand candidates would spend that time on the keystroke path with
/// nothing able to stop it.
const OFFERED_CANDIDATE_LIMIT: usize = 16;

fn plugin_hook(input: &StageInput, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    if input.offered.is_empty() {
        return candidates;
    }
    if input.offered.len() > OFFERED_CANDIDATE_LIMIT {
        tracing::warn!(
            offered = input.offered.len(),
            limit = OFFERED_CANDIDATE_LIMIT,
            "plugin host answered with more candidates than it was sent; ignoring the tail"
        );
    }

    let admitted: Vec<Suggestion> = input
        .offered
        .iter()
        .take(OFFERED_CANDIDATE_LIMIT)
        .filter(
            |offered| match admit(offered, input.reading, &input.priced_spans) {
                Ok(()) => true,
                Err(reason) => {
                    tracing::warn!(
                        text = %offered.text,
                        surface_count = offered.surface_count,
                        corresponding_count = offered.corresponding_count,
                        ?reason,
                        "dropped a candidate offered by the plugin host"
                    );
                    false
                }
            },
        )
        .cloned()
        .collect();

    insert_added(candidates, admitted)
}

// The engine can propose the same surface text more than once and only the
// first occurrence is kept. A HashSet of what has been seen rather than a
// scan of the output per candidate: the list is rebuilt on every keystroke,
// and the scan made that quadratic in a long candidate list for no reason.
fn dedup(candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.text.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    //! Pure-function tests: nothing here may reference an FFI symbol
    //! (see the warning in wrappers.rs — one import would make every test
    //! in the crate need the Swift runtime).

    use super::{Refusal, StageInput, admit, run};
    use shared::proto::Suggestion;

    fn suggestion(text: &str, subtext: &str) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            subtext: subtext.to_string(),
            corresponding_count: 1,
            surface_count: 1,
        }
    }

    /// A candidate as the engine reports one: it says how much of the
    /// reading it covers, in both units.
    fn spanning(text: &str, corresponding_count: i32, surface_count: i32) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            subtext: String::new(),
            corresponding_count,
            surface_count,
        }
    }

    fn texts(candidates: &[Suggestion]) -> Vec<&str> {
        candidates.iter().map(|s| s.text.as_str()).collect()
    }

    /// The pipeline with nothing offered — the default, since plugins are
    /// off unless the user turns them on.
    fn run_plain(reading: &str, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
        let input = StageInput::new(reading, &[], &candidates);
        run(&input, candidates)
    }

    fn run_offering(
        reading: &str,
        offered: &[Suggestion],
        candidates: Vec<Suggestion>,
    ) -> Vec<Suggestion> {
        let input = StageInput::new(reading, offered, &candidates);
        run(&input, candidates)
    }

    /// The spans of a candidate list, as `admit` receives them.
    fn spans_of(engine: &[Suggestion]) -> std::collections::HashSet<(i32, i32)> {
        engine
            .iter()
            .map(|c| (c.surface_count, c.corresponding_count))
            .collect()
    }

    #[test]
    fn duplicate_text_keeps_the_first_occurrence() {
        let out = run_plain(
            "きしゃ",
            vec![
                suggestion("記者", ""),
                suggestion("汽車", ""),
                suggestion("記者", ""),
            ],
        );

        assert_eq!(texts(&out), ["記者", "汽車"]);
    }

    #[test]
    fn order_is_preserved() {
        let out = run_plain(
            "あき",
            vec![
                suggestion("秋", ""),
                suggestion("空き", ""),
                suggestion("飽き", ""),
            ],
        );

        assert_eq!(texts(&out), ["秋", "空き", "飽き"]);
    }

    /// Candidates that render the same are duplicates even when their
    /// subtexts differ — the first one's subtext survives.
    #[test]
    fn subtext_difference_does_not_defeat_dedup() {
        let out = run_plain(
            "きしゃ",
            vec![
                suggestion("記者", "annotation A"),
                suggestion("記者", "annotation B"),
            ],
        );

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].subtext, "annotation A");
    }

    #[test]
    fn empty_list_stays_empty() {
        assert_eq!(run_plain("", vec![]), vec![]);
    }

    /// The default path, and the one that must cost nothing: with the
    /// feature off there is nothing offered, and the list comes through
    /// exactly as the engine ranked it.
    #[test]
    fn nothing_offered_leaves_the_list_alone() {
        let engine: Vec<Suggestion> = ["きょう", "今日", "境", "教", "橋"]
            .iter()
            .map(|t| spanning(t, 4, 3))
            .collect();

        let out = run_plain("きょう", engine.clone());

        assert_eq!(texts(&out), texts(&engine));
    }

    // ---- placement ----

    /// The placement rule on a list long enough for it to bite: the engine
    /// keeps the first four slots and the additions take the last visible
    /// one.
    #[test]
    fn added_candidates_land_on_the_last_visible_slot() {
        let engine: Vec<Suggestion> = ["きょう", "今日", "境", "教", "橋", "京", "卿"]
            .iter()
            .map(|t| spanning(t, 4, 3))
            .collect();

        let out = run_offering("きょう", &[spanning("2026/07/29", 4, 3)], engine);

        assert_eq!(
            texts(&out),
            ["きょう", "今日", "境", "教", "2026/07/29", "橋", "京", "卿"]
        );
    }

    /// Found on a live engine: its raw list repeats itself near the top,
    /// so placing at index 4 and deduplicating afterwards landed the
    /// addition at index 2. The rank has to be measured against the list
    /// the user is shown.
    #[test]
    fn the_rank_counts_rows_that_survive_dedup() {
        let engine: Vec<Suggestion> = ["きょう", "きょう", "今日", "今日", "境", "教", "橋"]
            .iter()
            .map(|t| spanning(t, 4, 3))
            .collect();

        let out = run_offering("きょう", &[spanning("2026/07/29", 4, 3)], engine);

        assert_eq!(
            texts(&out),
            ["きょう", "今日", "境", "教", "2026/07/29", "橋"]
        );
    }

    /// A host that answers with far more than it was sent must not make
    /// the keystroke path pay for it: checking happens outside the RPC
    /// timeout, so the bound has to be here.
    #[test]
    fn an_oversized_answer_is_read_only_as_far_as_it_could_matter() {
        let engine: Vec<Suggestion> = (0..6)
            .map(|i| spanning(&format!("候補{i}"), 4, 3))
            .collect();
        // every entry is admissible, so only the limit can stop them
        let offered: Vec<Suggestion> = (0..10_000)
            .map(|i| spanning(&format!("追加{i}"), 4, 3))
            .collect();

        let out = run_offering("きょう", &offered, engine);

        assert_eq!(
            texts(&out),
            [
                "候補0", "候補1", "候補2", "候補3", "追加0", "追加1", "追加2", "候補4", "候補5"
            ]
        );
    }

    /// A span the engine priced on a row dedup then removed is still a
    /// span the engine priced — and it is a row the host was shown, since
    /// it is sent the list before the pipeline touches it. Refusing it
    /// would make the public contract ("copy the counts from a candidate
    /// you were given") false in a way fail-open hides.
    #[test]
    fn a_span_from_a_row_dedup_removed_is_still_corroborated() {
        // two rows render the same and cover different spans; dedup keeps
        // only the first
        let engine = vec![
            spanning("きょう", 4, 3),
            spanning("今日", 4, 3),
            spanning("今日", 2, 1),
            spanning("境", 4, 3),
            spanning("教", 4, 3),
        ];

        let out = run_offering("きょう", &[spanning("追加", 2, 1)], engine);

        assert_eq!(
            texts(&out),
            ["きょう", "今日", "境", "教", "追加"],
            "the offered candidate copied a span the engine really produced"
        );
    }

    /// The invariant everything else is arranged around.
    #[test]
    fn the_top_candidate_is_never_displaced() {
        for engine_len in 0..8 {
            let engine: Vec<Suggestion> = (0..engine_len)
                .map(|i| spanning(&format!("候補{i}"), 4, 3))
                .collect();

            let out = run_offering("きょう", &[spanning("追加", 4, 3)], engine);

            match engine_len {
                // nothing from the engine means nothing to be behind an
                // addition, so there is nothing to add TO
                0 => assert!(out.is_empty(), "an empty list gains nothing"),
                _ => assert_eq!(out[0].text, "候補0", "list of {engine_len}"),
            }
        }
    }

    /// The rule stated on its own terms, not as a side effect of `admit`
    /// refusing everything when there is nothing to corroborate against.
    /// Called directly, because the pipeline cannot reach this state while
    /// that other rule holds — and the point is that it must stay safe if
    /// that rule is ever relaxed.
    #[test]
    fn nothing_is_placed_into_an_empty_list() {
        let out = super::insert_added(Vec::new(), vec![spanning("追加", 4, 3)]);

        assert!(
            out.is_empty(),
            "an addition must never become what Enter commits"
        );
    }

    #[test]
    fn a_short_list_appends() {
        let out = run_offering(
            "きょう",
            &[spanning("追加", 4, 3)],
            vec![spanning("今日", 4, 3)],
        );

        assert_eq!(texts(&out), ["今日", "追加"]);
    }

    /// The cap is on the pipeline, not on whoever wants in: no plugin can
    /// push the engine's ranking further down by offering more.
    #[test]
    fn no_more_than_the_cap_is_placed() {
        let engine: Vec<Suggestion> = (0..6)
            .map(|i| spanning(&format!("候補{i}"), 4, 3))
            .collect();
        let offered: Vec<Suggestion> = (0..5)
            .map(|i| spanning(&format!("追加{i}"), 4, 3))
            .collect();

        let out = run_offering("きょう", &offered, engine);

        assert_eq!(
            texts(&out),
            [
                "候補0", "候補1", "候補2", "候補3", "追加0", "追加1", "追加2", "候補4", "候補5"
            ]
        );
    }

    /// Dedup runs BEFORE this stage and cannot come back to tidy up, so
    /// an answer that repeats itself would reach the window repeating
    /// itself. Today one host says it twice; once several plugins are
    /// summed into one answer, two of them agreeing is the ordinary case.
    #[test]
    fn an_answer_that_repeats_itself_is_placed_once() {
        let engine: Vec<Suggestion> = (0..6)
            .map(|i| spanning(&format!("候補{i}"), 4, 3))
            .collect();
        let offered = [
            spanning("追加", 4, 3),
            spanning("追加", 4, 3),
            spanning("追加", 4, 3),
        ];

        let out = run_offering("きょう", &offered, engine);

        assert_eq!(
            texts(&out),
            ["候補0", "候補1", "候補2", "候補3", "追加", "候補4", "候補5"]
        );
    }

    /// The cap counts what is placed, not what was offered: three copies
    /// of one text must not spend the whole budget and crowd out a second
    /// distinct candidate behind them.
    #[test]
    fn duplicates_do_not_spend_the_cap() {
        let engine: Vec<Suggestion> = (0..6)
            .map(|i| spanning(&format!("候補{i}"), 4, 3))
            .collect();
        let offered = [
            spanning("A", 4, 3),
            spanning("A", 4, 3),
            spanning("A", 4, 3),
            spanning("B", 4, 3),
        ];

        let out = run_offering("きょう", &offered, engine);

        assert_eq!(
            texts(&out),
            [
                "候補0", "候補1", "候補2", "候補3", "A", "B", "候補4", "候補5"
            ]
        );
    }

    #[test]
    fn an_offer_the_engine_already_made_is_not_duplicated() {
        let out = run_offering(
            "きょう",
            &[spanning("今日", 4, 3)],
            vec![spanning("きょう", 4, 3), spanning("今日", 4, 3)],
        );

        assert_eq!(texts(&out), ["きょう", "今日"]);
    }

    // ---- what the host is allowed to put in front of the user ----

    #[test]
    fn a_corroborated_span_is_admitted() {
        let engine = [spanning("今日", 4, 3)];

        assert_eq!(
            admit(&spanning("2026/07/29", 4, 3), "きょう", &spans_of(&engine)),
            Ok(())
        );
    }

    #[test]
    fn empty_text_is_refused() {
        let engine = [spanning("今日", 4, 3)];

        assert_eq!(
            admit(&spanning("", 4, 3), "きょう", &spans_of(&engine)),
            Err(Refusal::EmptyText)
        );
    }

    /// A candidate claiming more kana than the reading has would make the
    /// commit spend kana that are not there.
    #[test]
    fn a_surface_count_past_the_reading_is_refused() {
        let engine = [spanning("今日", 4, 3)];

        assert_eq!(
            admit(&spanning("嘘", 4, 4), "きょう", &spans_of(&engine)),
            Err(Refusal::SurfaceCountOutOfRange)
        );
        assert_eq!(
            admit(&spanning("嘘", 4, 0), "きょう", &spans_of(&engine)),
            Err(Refusal::SurfaceCountOutOfRange)
        );
    }

    #[test]
    fn a_negative_keystroke_count_is_refused() {
        let engine = [spanning("今日", 4, 3)];

        assert_eq!(
            admit(&spanning("嘘", -1, 3), "きょう", &spans_of(&engine)),
            Err(Refusal::NegativeCorrespondingCount)
        );
    }

    /// The rule a plugin is most likely to break honestly: the span looks
    /// reasonable but no candidate the engine produced claims it, so
    /// nothing corroborates the keystroke count. Committing it would leave
    /// the client's raw input out of step with the screen.
    #[test]
    fn a_span_no_engine_candidate_claims_is_refused() {
        let engine = [spanning("今日", 4, 3)];

        assert_eq!(
            admit(&spanning("嘘", 5, 3), "きょう", &spans_of(&engine)),
            Err(Refusal::SpanNotCorroborated)
        );
        assert_eq!(
            admit(&spanning("嘘", 4, 2), "きょう", &spans_of(&engine)),
            Err(Refusal::SpanNotCorroborated)
        );
    }

    /// A partial span is fine as long as the engine priced it — a plugin
    /// may offer text for a clause the engine also found.
    #[test]
    fn a_partial_span_the_engine_claims_is_admitted() {
        let engine = [spanning("今日", 4, 3), spanning("木", 2, 1)];

        assert_eq!(
            admit(&spanning("樹", 2, 1), "きょう", &spans_of(&engine)),
            Ok(())
        );
    }

    /// The whole point, end to end: a host that answers with nonsense
    /// changes nothing the user sees.
    #[test]
    fn a_hostile_answer_cannot_reach_the_user() {
        let engine = vec![spanning("きょう", 4, 3), spanning("今日", 4, 3)];
        let offered = [
            spanning("", 4, 3),
            spanning("span past the reading", 4, 99),
            spanning("negative", -3, 3),
            spanning("uncorroborated", 7, 3),
        ];

        let out = run_offering("きょう", &offered, engine.clone());

        assert_eq!(texts(&out), texts(&engine));
    }
}
