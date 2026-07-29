//! The plugins that ship with the IME, dispatched by an exhaustive match.
//!
//! Nothing is loaded dynamically here. Third-party code arrives later, in
//! a sandbox inside this process; the shape of this module — a fixed list,
//! each entry answering "what would you add?" and nothing else — is the
//! shape that boundary has to fit through.
//!
//! A plugin only ever ADDS. It cannot remove, reorder, or rank: it is
//! handed the list so it can see what is already offered and copy a span
//! from it, and it answers with candidates the core then places.

use chrono::{Datelike, NaiveDate, TimeDelta};

use shared::proto::PluginCandidate;

/// One plugin that ships with the IME.
pub(crate) enum BuiltinPlugin {
    /// Offers the calendar date for a reading that names a day.
    CalendarDate,
}

/// Every builtin, in the order they are asked. The core caps how many
/// added candidates it will place, so this order decides who wins when
/// several plugins want in at once.
const BUILTINS: &[BuiltinPlugin] = &[BuiltinPlugin::CalendarDate];

/// What a plugin is given besides the candidate list.
///
/// The clock is read once, at the top of a request, rather than by the
/// plugin that wants it: a plugin that called `Local::now` itself could
/// not be tested, and two plugins in one request could disagree about
/// what day it is across a midnight boundary.
struct PluginInput<'a> {
    reading: &'a str,
    today: NaiveDate,
}

fn offer(
    plugin: &BuiltinPlugin,
    input: &PluginInput,
    candidates: &[PluginCandidate],
) -> Vec<PluginCandidate> {
    match plugin {
        BuiltinPlugin::CalendarDate => calendar_date(input, candidates),
    }
}

/// Everything the builtins want to add, in `BUILTINS` order.
pub(crate) fn run(reading: &str, candidates: &[PluginCandidate]) -> Vec<PluginCandidate> {
    run_with(
        &PluginInput {
            reading,
            today: chrono::Local::now().date_naive(),
        },
        candidates,
    )
}

fn run_with(input: &PluginInput, candidates: &[PluginCandidate]) -> Vec<PluginCandidate> {
    BUILTINS
        .iter()
        .flat_map(|plugin| offer(plugin, input, candidates))
        .collect()
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

fn calendar_date(input: &PluginInput, candidates: &[PluginCandidate]) -> Vec<PluginCandidate> {
    let Some((_, offset)) = DAY_READINGS.iter().find(|(r, _)| *r == input.reading) else {
        return Vec::new();
    };
    let Some(date) = TimeDelta::try_days(*offset).and_then(|d| input.today.checked_add_signed(d))
    else {
        return Vec::new();
    };

    // How much of the reading a candidate covers has to be exactly right:
    // `surface_count` is what the commit spends, and `corresponding_count`
    // is what the client drops from its own keystroke buffer. The kana
    // count is knowable — the reading matched a day name in full. The
    // KEYSTROKE count is not: the same reading can be typed as kyou or
    // kilyou, and only the engine tracked which. So it is copied from a
    // candidate the engine already reported for this same span, and when
    // there is none this offers nothing. The core rejects a candidate
    // whose span it cannot corroborate anyway; getting it wrong shows up
    // as stale romaji after a commit, not as a bad candidate.
    let surface_count = input.reading.chars().count() as i32;
    let Some(corresponding_count) = candidates
        .iter()
        .find(|c| c.surface_count == surface_count)
        .map(|c| c.corresponding_count)
    else {
        return Vec::new();
    };

    formatted_dates(date)
        .into_iter()
        .map(|text| PluginCandidate {
            text,
            subtext: "日付".to_string(),
            corresponding_count,
            surface_count,
        })
        .collect()
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
/// not 1年. `None` before that date: this only ever formats a day near
/// today, so an earlier date means the machine clock is wrong, and
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

#[cfg(test)]
mod tests {
    use super::{PluginInput, run, run_with};
    use chrono::{Datelike, NaiveDate};
    use shared::proto::PluginCandidate;

    fn spanning(text: &str, corresponding_count: i32, surface_count: i32) -> PluginCandidate {
        PluginCandidate {
            text: text.to_string(),
            subtext: String::new(),
            corresponding_count,
            surface_count,
        }
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("a real calendar date")
    }

    fn run_on(reading: &str, today: NaiveDate, candidates: &[PluginCandidate]) -> Vec<String> {
        run_with(&PluginInput { reading, today }, candidates)
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    #[test]
    fn a_day_reading_is_offered_the_date() {
        let out = run_on(
            "きょう",
            date(2026, 7, 29),
            &[spanning("今日", 4, 3), spanning("京", 4, 3)],
        );

        assert_eq!(out, ["2026/07/29", "2026年7月29日", "令和8年7月29日"]);
    }

    /// The answer carries only what to add — the candidates it was given
    /// are not echoed back, because the core owns the list.
    #[test]
    fn nothing_the_caller_sent_comes_back() {
        let out = run_on("きょう", date(2026, 7, 29), &[spanning("今日", 4, 3)]);

        assert!(!out.contains(&"今日".to_string()));
    }

    #[test]
    fn the_span_is_copied_from_the_caller() {
        let added = run_with(
            &PluginInput {
                reading: "きょう",
                today: date(2026, 7, 29),
            },
            // kilyou: six keystrokes for the same three kana
            &[spanning("今日", 6, 3)],
        );

        assert!(
            added
                .iter()
                .all(|c| c.corresponding_count == 6 && c.surface_count == 3),
            "a made-up keystroke count leaves stale romaji after a commit"
        );
    }

    /// When several candidates cover the same kana but disagree about
    /// keystrokes, the first — the engine's highest-ranked — is the one
    /// copied. Any of them would be corroborated by the core, so this is
    /// a choice rather than a constraint, and it is pinned so a rewrite
    /// has to make it again on purpose.
    #[test]
    fn the_span_comes_from_the_highest_ranked_candidate_that_covers_the_reading() {
        let added = run_with(
            &PluginInput {
                reading: "きょう",
                today: date(2026, 7, 29),
            },
            &[
                spanning("きょう", 3, 3),
                spanning("今日", 4, 3),
                spanning("京", 6, 3),
            ],
        );

        assert!(
            added.iter().all(|c| c.corresponding_count == 3),
            "the first covering candidate decides"
        );
    }

    #[test]
    fn nothing_is_offered_without_a_candidate_covering_the_reading() {
        assert!(run_on("きょう", date(2026, 7, 29), &[spanning("きょ", 3, 2)]).is_empty());
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
            let len = reading.chars().count() as i32;
            let out = run_on(reading, date(2026, 7, 29), &[spanning("x", 4, len)]);

            assert_eq!(out[0], expected, "reading {reading}");
        }
    }

    #[test]
    fn a_day_offset_crosses_the_year_boundary() {
        let out = run_on("あした", date(2026, 12, 31), &[spanning("明日", 6, 3)]);

        assert_eq!(out[0], "2027/01/01");
    }

    #[test]
    fn an_ordinary_reading_is_offered_nothing() {
        assert!(run_on("きしゃ", date(2026, 7, 29), &[spanning("記者", 5, 3)]).is_empty());
        assert!(run_on("きょうは", date(2026, 7, 29), &[spanning("今日は", 6, 4)]).is_empty());
    }

    #[test]
    fn the_first_year_of_reiwa_is_gannen() {
        let out = run_on("きょう", date(2019, 5, 1), &[spanning("今日", 4, 3)]);

        assert_eq!(out[2], "令和元年5月1日");
    }

    #[test]
    fn a_date_before_reiwa_gets_no_era_candidate() {
        let out = run_on("きょう", date(2019, 4, 30), &[spanning("今日", 4, 3)]);

        assert_eq!(out, ["2019/04/30", "2019年4月30日"]);
    }

    /// The production entry point reads the real clock; everything else is
    /// pinned, so this is the one test that proves `run` is wired to it.
    #[test]
    fn run_uses_the_current_date() {
        let slash = |d: NaiveDate| format!("{:04}/{:02}/{:02}", d.year(), d.month(), d.day());
        // bracketing the call rather than taking one reading: the clock can
        // roll over mid-test, and a CI failure at midnight would say
        // nothing about the code
        let before = chrono::Local::now().date_naive();
        let added = run("きょう", &[spanning("今日", 4, 3)]);
        let after = chrono::Local::now().date_naive();

        assert!([before, after].map(slash).contains(&added[0].text));
    }
}
