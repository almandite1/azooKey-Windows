//! The candidate post-processing pipeline: every RPC that returns a
//! candidate list flows through `run` (called from service.rs::composed,
//! the single merge point), so a new transformation is a new
//! `CandidateStage` variant plus its match arm here — no other file.
//!
//! Stages are pure functions over (reading, candidates); nothing here may
//! touch the FFI, so the whole module is testable without a Swift runtime.

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
}

/// The fixed order every candidate list goes through.
const STAGES: &[CandidateStage] = &[CandidateStage::Dedup];

/// Applies one stage. `reading` is the hiragana the candidates convert;
/// Dedup has no use for it, but it is part of the stage contract so that
/// reading-dependent stages slot in without touching the signature.
fn apply(stage: &CandidateStage, reading: &str, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    let _ = reading;
    match stage {
        CandidateStage::Dedup => dedup(candidates),
    }
}

/// Runs the full pipeline in the order `STAGES` fixes.
pub(crate) fn run(reading: &str, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    STAGES.iter().fold(candidates, |candidates, stage| {
        apply(stage, reading, candidates)
    })
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

    use super::run;
    use shared::proto::Suggestion;

    fn suggestion(text: &str, subtext: &str) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            subtext: subtext.to_string(),
            corresponding_count: 1,
            surface_count: 1,
        }
    }

    #[test]
    fn duplicate_text_keeps_the_first_occurrence() {
        let out = run(
            "きしゃ",
            vec![
                suggestion("記者", ""),
                suggestion("汽車", ""),
                suggestion("記者", ""),
            ],
        );

        assert_eq!(
            out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["記者", "汽車"]
        );
    }

    #[test]
    fn order_is_preserved() {
        let out = run(
            "あき",
            vec![
                suggestion("秋", ""),
                suggestion("空き", ""),
                suggestion("飽き", ""),
            ],
        );

        assert_eq!(
            out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["秋", "空き", "飽き"]
        );
    }

    /// Candidates that render the same are duplicates even when their
    /// subtexts differ — the first one's subtext survives.
    #[test]
    fn subtext_difference_does_not_defeat_dedup() {
        let out = run(
            "きょう",
            vec![
                suggestion("今日", "annotation A"),
                suggestion("今日", "annotation B"),
            ],
        );

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].subtext, "annotation A");
    }

    #[test]
    fn empty_list_stays_empty() {
        assert_eq!(run("", vec![]), vec![]);
    }
}
