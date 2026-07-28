//! The candidate post-processing pipeline: every RPC that returns a
//! candidate list flows through `run` (called from service.rs::composed,
//! the single merge point), so a new transformation is a new
//! `CandidateStage` variant plus its match arm here — no other file.
//!
//! Stages are pure functions over (reading, candidates); nothing here may
//! touch the FFI, so the whole module is testable without a Swift runtime.

use chrono::{Datelike, NaiveDate, TimeDelta};

use shared::proto::Suggestion;

/// One transformation of the candidate list. The match in `apply` is
/// exhaustive on purpose: adding a variant without deciding what it does
/// is a compile error, not a silent no-op.
pub(crate) enum CandidateStage {
    /// Offers the calendar date for a reading that names a day.
    CalendarDate,
    /// Drops candidates whose surface text has already appeared, keeping
    /// the first (highest-ranked) occurrence. Subtext is deliberately not
    /// part of the key: two candidates that render the same are duplicates
    /// to the user regardless of how their annotations differ.
    Dedup,
}

/// The fixed order every candidate list goes through.
///
/// Dedup runs FIRST, and that is load-bearing rather than tidy: a rank
/// means a position in the list the user actually sees, and the engine's
/// raw list repeats itself near the top. Inserting at index 4 before
/// deduplicating put the date at index 2 on a live engine — the rank was
/// measured against rows that were about to be removed.
const STAGES: &[CandidateStage] = &[CandidateStage::Dedup, CandidateStage::CalendarDate];

/// Everything a stage may need besides the candidate list.
///
/// Built once per run, and the clock is read here rather than inside the
/// stage that wants it: a stage that called `Local::now` itself could not
/// be tested, and two stages in one run could otherwise disagree about
/// what day it is across a midnight boundary.
struct StageInput<'a> {
    /// The hiragana the candidates convert.
    reading: &'a str,
    today: NaiveDate,
}

/// Applies one stage.
fn apply(
    stage: &CandidateStage,
    input: &StageInput,
    candidates: Vec<Suggestion>,
) -> Vec<Suggestion> {
    match stage {
        CandidateStage::CalendarDate => calendar_date(input, candidates),
        CandidateStage::Dedup => dedup(candidates),
    }
}

/// Runs the full pipeline in the order `STAGES` fixes.
pub(crate) fn run(reading: &str, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    run_with(
        &StageInput {
            reading,
            today: chrono::Local::now().date_naive(),
        },
        candidates,
    )
}

fn run_with(input: &StageInput, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    STAGES.iter().fold(candidates, |candidates, stage| {
        apply(stage, input, candidates)
    })
}

/// Where a candidate the pipeline INVENTED goes in the list.
///
/// The candidate window shows five at a time and pages by five
/// (`MAX_VISIBLE_CANDIDATES` in crates/ui/assets/candidate.js), so index 4
/// is the last slot a user sees without paging. Appending instead put the
/// date at position 331 of 334 for きょう — correct, and unreachable.
///
/// Revisit this if the window's page size changes; the two numbers are
/// related by "the last visible slot", not by coincidence.
const ADDED_CANDIDATE_RANK: usize = 4;

/// How many invented candidates the pipeline will place, in total, per
/// list. A bound on how far the engine's own ranking can be pushed down,
/// and the number a plugin must not be able to raise (see the policy note
/// on `insert_added`).
const MAX_ADDED_CANDIDATES: usize = 3;

/// Places candidates that no conversion produced.
///
/// THE POLICY, which stages and (later) plugins do not get a say in:
///
/// - index 0 is never displaced. It is what Enter commits, so moving it
///   would change what typing does — the one thing none of this may do.
///   With a non-empty list the clamp below cannot reach 0.
/// - everything else added goes at one fixed rank, decided here. A stage
///   that could ask for a rank would be a stage that competes with the
///   next stage for the top of the list, and the user would arbitrate a
///   fight they never asked to have.
/// - at most `MAX_ADDED_CANDIDATES` survive, so the bound holds however
///   many stages want in.
/// - a text the list already carries is not added again. Deduplicating
///   here rather than in a later Dedup pass is what lets the rank be
///   honest: the engine's own list is already deduplicated by the time
///   anything is placed into it.
///
/// The rank is clamped to the list, so a short list appends rather than
/// leaving a gap.
fn insert_added(mut candidates: Vec<Suggestion>, added: Vec<Suggestion>) -> Vec<Suggestion> {
    let existing: std::collections::HashSet<&str> =
        candidates.iter().map(|c| c.text.as_str()).collect();
    let fresh: Vec<Suggestion> = added
        .into_iter()
        .filter(|c| !existing.contains(c.text.as_str()))
        .take(MAX_ADDED_CANDIDATES)
        .collect();

    let at = ADDED_CANDIDATE_RANK.min(candidates.len());
    candidates.splice(at..at, fresh);
    candidates
}

/// Readings that name a day, and how far that day is from today. Only an
/// exact whole-reading match counts: "きょうは" is the start of a sentence,
/// not a request for the date, and matching a prefix would also leave the
/// span ambiguous.
const DAY_READINGS: &[(&str, i64)] = &[
    ("きょう", 0),
    ("あす", 1),
    ("あした", 1),
    ("きのう", -1),
    ("あさって", 2),
    ("おととい", -2),
];

/// Offers the calendar date as extra candidates when the whole reading
/// names a day. Where they land is `insert_added`'s decision, not this
/// stage's.
fn calendar_date(input: &StageInput, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
    let Some((_, offset)) = DAY_READINGS.iter().find(|(r, _)| *r == input.reading) else {
        return candidates;
    };
    let Some(date) = TimeDelta::try_days(*offset).and_then(|d| input.today.checked_add_signed(d))
    else {
        return candidates;
    };

    // How much of the reading a candidate covers has to be exactly right:
    // `surface_count` is what ShrinkText spends, and `corresponding_count`
    // is what the client drops from its own keystroke buffer. The kana
    // count we know — the whole reading, since the match above was against
    // all of it. The KEYSTROKE count we do not: the same reading can be
    // typed as kyou or kilyou, and only the engine tracked which. So take
    // it from a candidate the engine already reported for this same span,
    // and offer nothing at all when there is none. Inventing a number here
    // is how a candidate leaves stale romaji behind after a commit.
    let surface_count = input.reading.chars().count() as i32;
    let Some(corresponding_count) = candidates
        .iter()
        .find(|c| c.surface_count == surface_count)
        .map(|c| c.corresponding_count)
    else {
        return candidates;
    };

    let dates = formatted_dates(date)
        .into_iter()
        .map(|text| Suggestion {
            text,
            subtext: "日付".to_string(),
            corresponding_count,
            surface_count,
        })
        .collect();
    insert_added(candidates, dates)
}

fn formatted_dates(date: NaiveDate) -> Vec<String> {
    let mut formats = vec![
        format!("{:04}/{:02}/{:02}", date.year(), date.month(), date.day()),
        format!("{}年{}月{}日", date.year(), date.month(), date.day()),
    ];
    if let Some(year) = reiwa_year(date) {
        formats.push(format!("令和{}年{}月{}日", year, date.month(), date.day()));
    }
    formats
}

/// The Reiwa era began on 2019-05-01 and its first year is written 元年,
/// not 1年. `None` before that date: this stage only ever formats a day
/// near today, so an earlier date means the machine clock is wrong, and
/// silently labelling 2018 as 令和0年 would be worse than saying nothing.
fn reiwa_year(date: NaiveDate) -> Option<String> {
    if date < NaiveDate::from_ymd_opt(2019, 5, 1)? {
        return None;
    }
    Some(match date.year() - 2018 {
        1 => "元".to_string(),
        year => year.to_string(),
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

    use super::{StageInput, run, run_with};
    use chrono::{Datelike, NaiveDate};
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

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("a real calendar date")
    }

    /// The pipeline with the clock pinned, so a test never depends on the
    /// day it runs.
    fn run_on(reading: &str, today: NaiveDate, candidates: Vec<Suggestion>) -> Vec<Suggestion> {
        run_with(&StageInput { reading, today }, candidates)
    }

    fn texts(candidates: &[Suggestion]) -> Vec<&str> {
        candidates.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn duplicate_text_keeps_the_first_occurrence() {
        let out = run_on(
            "きしゃ",
            date(2026, 7, 29),
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
        let out = run_on(
            "あき",
            date(2026, 7, 29),
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
        let out = run_on(
            "きしゃ",
            date(2026, 7, 29),
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
        assert_eq!(run_on("", date(2026, 7, 29), vec![]), vec![]);
    }

    #[test]
    fn a_day_reading_gains_the_calendar_date() {
        let out = run_on(
            "きょう",
            date(2026, 7, 29),
            vec![spanning("今日", 4, 3), spanning("京", 4, 3)],
        );

        assert_eq!(
            texts(&out),
            [
                "今日",
                "京",
                "2026/07/29",
                "2026年7月29日",
                "令和8年7月29日"
            ],
            "the engine's own candidates keep the top of the list"
        );
        assert!(out[2..].iter().all(|s| s.subtext == "日付"));
    }

    /// The span has to be exactly right or committing the candidate
    /// desyncs the client: kana for ShrinkText, keystrokes for the raw
    /// input buffer. Both are copied from what the engine said about the
    /// same reading — here a reading typed as `kilyou`, six keystrokes.
    #[test]
    fn added_candidates_span_what_the_engine_said_they_span() {
        let out = run_on("きょう", date(2026, 7, 29), vec![spanning("今日", 6, 3)]);

        assert!(
            out[1..]
                .iter()
                .all(|s| s.corresponding_count == 6 && s.surface_count == 3),
            "a made-up keystroke count would leave stale romaji after a commit"
        );
    }

    /// No candidate covers the whole reading, so nothing here knows what
    /// the span costs in keystrokes — offer nothing rather than guess.
    #[test]
    fn nothing_is_added_without_a_candidate_covering_the_reading() {
        let out = run_on("きょう", date(2026, 7, 29), vec![spanning("きょ", 3, 2)]);

        assert_eq!(texts(&out), ["きょ"]);
    }

    #[test]
    fn offsets_land_on_the_right_day() {
        for (reading, expected) in [
            ("きのう", "2026/07/28"),
            ("あした", "2026/07/30"),
            ("あす", "2026/07/30"),
            ("あさって", "2026/07/31"),
            ("おととい", "2026/07/27"),
        ] {
            let reading_len = reading.chars().count() as i32;
            let out = run_on(
                reading,
                date(2026, 7, 29),
                vec![spanning("x", 4, reading_len)],
            );

            assert_eq!(out[1].text, expected, "reading {reading}");
        }
    }

    #[test]
    fn a_day_offset_crosses_the_year_boundary() {
        let out = run_on("あした", date(2026, 12, 31), vec![spanning("明日", 6, 3)]);

        assert_eq!(out[1].text, "2027/01/01");
        assert_eq!(out[2].text, "2027年1月1日");
    }

    /// Only the whole reading counts: this one is the start of a sentence.
    #[test]
    fn a_reading_that_merely_starts_with_a_day_is_untouched() {
        let out = run_on(
            "きょうは",
            date(2026, 7, 29),
            vec![spanning("今日は", 6, 4)],
        );

        assert_eq!(texts(&out), ["今日は"]);
    }

    #[test]
    fn an_ordinary_reading_is_untouched() {
        let out = run_on("きしゃ", date(2026, 7, 29), vec![spanning("記者", 5, 3)]);

        assert_eq!(texts(&out), ["記者"]);
    }

    /// The placement rule, on a list long enough for it to bite: the
    /// engine keeps the first four slots and the dates take the last
    /// visible one. Appending instead is what put the date at 331 of 334
    /// for きょう on a live engine — right, and out of reach.
    #[test]
    fn added_candidates_land_on_the_last_visible_slot() {
        let engine: Vec<Suggestion> = ["きょう", "今日", "境", "教", "橋", "京", "卿"]
            .iter()
            .map(|t| spanning(t, 4, 3))
            .collect();

        let out = run_on("きょう", date(2026, 7, 29), engine);

        assert_eq!(
            texts(&out),
            [
                "きょう",
                "今日",
                "境",
                "教",
                "2026/07/29",
                "2026年7月29日",
                "令和8年7月29日",
                "橋",
                "京",
                "卿"
            ]
        );
    }

    /// Found on a live engine: its raw list repeats itself near the top,
    /// so placing at index 4 and deduplicating afterwards landed the date
    /// at index 2. The rank has to be measured against the list the user
    /// is shown, which is why Dedup runs before the stages that add.
    #[test]
    fn the_rank_counts_rows_that_survive_dedup() {
        let engine: Vec<Suggestion> = ["きょう", "きょう", "今日", "今日", "境", "教", "橋"]
            .iter()
            .map(|t| spanning(t, 4, 3))
            .collect();

        let out = run_on("きょう", date(2026, 7, 29), engine);

        assert_eq!(
            texts(&out),
            [
                "きょう",
                "今日",
                "境",
                "教",
                "2026/07/29",
                "2026年7月29日",
                "令和8年7月29日",
                "橋"
            ]
        );
    }

    /// The invariant everything else is arranged around.
    #[test]
    fn the_top_candidate_is_never_displaced() {
        for engine_len in 1..8 {
            let engine: Vec<Suggestion> = (0..engine_len)
                .map(|i| spanning(&format!("候補{i}"), 4, 3))
                .collect();

            let out = run_on("きょう", date(2026, 7, 29), engine);

            assert_eq!(out[0].text, "候補0", "list of {engine_len}");
        }
    }

    /// A list shorter than the rank appends rather than leaving a gap.
    #[test]
    fn a_short_list_appends() {
        let out = run_on("きょう", date(2026, 7, 29), vec![spanning("今日", 4, 3)]);

        assert_eq!(
            texts(&out),
            ["今日", "2026/07/29", "2026年7月29日", "令和8年7月29日"]
        );
    }

    /// The bound is on the pipeline, not on the stage that wants in: a
    /// stage offering more than the cap gets the cap, so no future stage
    /// (or plugin) can push the engine's ranking further down by asking.
    #[test]
    fn no_more_than_the_cap_is_placed() {
        let engine: Vec<Suggestion> = (0..6)
            .map(|i| spanning(&format!("候補{i}"), 4, 3))
            .collect();
        let added: Vec<Suggestion> = (0..5)
            .map(|i| spanning(&format!("追加{i}"), 4, 3))
            .collect();

        let out = super::insert_added(engine, added);

        assert_eq!(
            texts(&out),
            [
                "候補0", "候補1", "候補2", "候補3", "追加0", "追加1", "追加2", "候補4", "候補5"
            ]
        );
    }

    /// The placement helper drops what the list already carries, so the
    /// date stage does not have to know what the engine offered.
    #[test]
    fn a_date_the_engine_already_proposed_is_not_duplicated() {
        let out = run_on(
            "きょう",
            date(2026, 7, 29),
            vec![spanning("今日", 4, 3), spanning("2026/07/29", 4, 3)],
        );

        assert_eq!(
            texts(&out),
            ["今日", "2026/07/29", "2026年7月29日", "令和8年7月29日"]
        );
    }

    #[test]
    fn the_first_year_of_reiwa_is_gannen() {
        let out = run_on("きょう", date(2019, 5, 1), vec![spanning("今日", 4, 3)]);

        assert_eq!(out[3].text, "令和元年5月1日");
    }

    /// A clock set before the era began gets the two plain formats and no
    /// era line, rather than 令和0年.
    #[test]
    fn a_date_before_reiwa_gets_no_era_candidate() {
        let out = run_on("きょう", date(2019, 4, 30), vec![spanning("今日", 4, 3)]);

        assert_eq!(texts(&out), ["今日", "2019/04/30", "2019年4月30日"]);
    }

    /// The production entry point reads the real clock; everything else is
    /// pinned, so this is the one test that proves `run` is wired to it.
    #[test]
    fn run_uses_the_current_date() {
        let slash_format =
            |d: NaiveDate| format!("{:04}/{:02}/{:02}", d.year(), d.month(), d.day());
        // bracketing the call rather than taking one reading: the clock can
        // roll over to the next day mid-test, and a CI failure at midnight
        // would say nothing about the code
        let before = chrono::Local::now().date_naive();
        let out = run("きょう", vec![spanning("今日", 4, 3)]);
        let after = chrono::Local::now().date_naive();

        assert!(
            [before, after].map(slash_format).contains(&out[1].text),
            "run must read the real clock, got {}",
            out[1].text
        );
    }
}
