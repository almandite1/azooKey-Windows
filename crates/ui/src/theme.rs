//! User theming: `%APPDATA%\Azookey\theme.json` overrides the design
//! tokens in assets/theme.css.
//!
//! Data only. There is no plugin host in this path and no code of any
//! kind — a theme is a set of colours, and this module's job is to make
//! sure that is all it can ever be.
//!
//! That guarantee is the whole design, because the values end up inside a
//! `<style>` element in the same webview that shows what the user is
//! typing. Any local process can write this file, and a value like
//! `red; } body { background: url(http://…) }` would close the
//! declaration and open a rule of its own — CSS injection into the IME's
//! own window. So neither half of an override is trusted:
//!
//! * the KEY must be one of the tokens theme.css actually defines, and
//! * the VALUE must parse as a hex colour and nothing else.
//!
//! Everything else is dropped with a warning. The emitted CSS is then
//! constrained by construction rather than by escaping, which is the only
//! version of this that stays true when someone edits it later.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The tokens a theme may set — every custom property `assets/theme.css`
/// defines, and nothing else.
///
/// Keep in step with that file: a token added there but not here simply
/// cannot be themed, which is a harmless kind of wrong. The reverse — a
/// name here that theme.css does not use — is also harmless, since the
/// override would just never apply. Neither can widen what a theme is
/// able to say.
const THEMEABLE: &[&str] = &[
    "window-bg",
    "window-border",
    "text",
    "text-strong",
    "text-muted",
    "accent",
    "selected-bg",
    "selected-outline",
    "scrollbar-thumb",
];

const THEME_FILENAME: &str = "theme.json";

/// The two blocks of `assets/theme.css` a theme can override. Both
/// optional: a file that sets only `light` leaves dark mode alone, which
/// is better than a half-applied theme.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Theme {
    pub light: BTreeMap<String, String>,
    pub dark: BTreeMap<String, String>,
}

impl Theme {
    /// Reads and validates the user's theme, or returns an empty one.
    ///
    /// Never fails: a missing file is the normal case, and a broken one
    /// must leave the IME looking like itself rather than stop it
    /// starting.
    pub fn read() -> Self {
        Theme::read_in(shared::config_root())
    }

    /// Takes the config directory explicitly so the file-handling paths
    /// can be tested without touching `%APPDATA%`.
    ///
    /// The read happens on a thread this one will wait only so long for.
    /// A size limit bounds a big file; nothing in `std` bounds a file that
    /// never answers, and on Windows an unprivileged process can leave a
    /// junction here pointing at a share that is gone. Both are the same
    /// failure from ui.exe's side — startup that does not finish — and
    /// only the second one needs a clock. A thread left blocked in a
    /// syscall costs a handle until the process exits, which is the
    /// cheapest thing on offer.
    fn read_in(config_root: Option<std::path::PathBuf>) -> Self {
        let Some(path) = config_root.map(|dir| dir.join(THEME_FILENAME)) else {
            return Theme::default();
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let reading = path.clone();
        std::thread::spawn(move || {
            // the receiver is gone on timeout; nothing to do about it
            let _ = tx.send(read_bounded(&reading));
        });

        match rx.recv_timeout(READ_BUDGET) {
            Ok(Ok(Some(text))) => Theme::parse(&text),
            // no file is the ordinary state and says nothing worth saying
            Ok(Ok(None)) => Theme::default(),
            Ok(Err(e)) => {
                eprintln!("failed to read {}: {e}", path.display());
                Theme::default()
            }
            Err(_) => {
                eprintln!(
                    "reading {} took longer than {READ_BUDGET:?}; ignoring it",
                    path.display()
                );
                Theme::default()
            }
        }
    }

    /// Parses a theme document and drops everything that is not a known
    /// token set to a hex colour.
    ///
    /// Two granularities, and the difference is serde's rather than a
    /// choice made here: a value of the wrong TYPE (a number, an array)
    /// fails the whole document, so the file is discarded; a value of the
    /// right type that this module then refuses (a colour that is not
    /// hex, a token that does not exist) costs only that entry. A user
    /// whose file has one bad colour keeps the rest of their theme; one
    /// whose file has `"window-bg": 5` gets the shipped colours back. Both
    /// are stated in the log.
    fn parse(text: &str) -> Self {
        // Windows editors readily save UTF-8 with a BOM and serde_json
        // rejects one — the same courtesy settings.json gets.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let raw: Theme = match serde_json::from_str(text) {
            Ok(theme) => theme,
            Err(e) => {
                eprintln!("invalid {THEME_FILENAME}: {e}");
                return Theme::default();
            }
        };
        Theme {
            light: sanitize(raw.light, "light"),
            dark: sanitize(raw.dark, "dark"),
        }
    }

    /// The `<style>` body to place AFTER theme.css, so these win by
    /// cascade order rather than by `!important` — a theme should lose to
    /// nothing and override nothing it was not given.
    ///
    /// BOTH blocks are wrapped in a media query, including the light one,
    /// and that is the whole correctness of this function. A bare `:root`
    /// here would be placed after theme.css's own
    /// `@media (prefers-color-scheme: dark)` block, and since the two have
    /// the same specificity the later one wins — so a light-only theme
    /// would paint its light colours over dark mode as well, leaving the
    /// shipped dark `--text` on the user's light background. Scoping the
    /// light block says what it means, and lets a file that names only one
    /// scheme leave the other exactly as it shipped.
    ///
    /// Empty when there is nothing to say, which keeps the generated page
    /// byte-identical to the untouched one on the overwhelmingly common
    /// path of no theme at all.
    pub fn to_css(&self) -> String {
        let mut css = String::new();
        if !self.light.is_empty() {
            css.push_str(&scheme_block("light", &self.light));
        }
        if !self.dark.is_empty() {
            css.push_str(&scheme_block("dark", &self.dark));
        }
        css
    }
}

/// The most a theme may be. Colours for nine tokens do not need more, and
/// the number is the point rather than the value.
///
/// This read happens on ui.exe's startup path, before the webview exists,
/// and the file sits in a directory any process running as the user can
/// write. Reading it without a bound is the one way a DATA-only add-on
/// could reach the core: ui.exe blocked here answers no health check, the
/// watchdog kills and restarts it, five times, and then its supervisor —
/// the fatal kind, because an IME with no candidate window is not an IME —
/// ends the launcher, which through the job object takes the engine down
/// with it. A theme must not be able to do that.
const MAX_THEME_BYTES: u64 = 64 * 1024;

/// How long ui.exe will wait for the file before deciding it has no
/// theme. Generous for a local read of at most 64 KB, and short next to
/// the watchdog's startup grace.
const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// `Ok(None)` when there is no file. A file larger than
/// [`MAX_THEME_BYTES`] is an error rather than a truncated read: half a
/// JSON document would parse as nothing anyway, and saying so names the
/// actual problem.
fn read_bounded(path: &std::path::Path) -> std::io::Result<Option<String>> {
    use std::io::Read as _;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    // one byte past the limit, so "exactly at the limit" and "over it"
    // are distinguishable without a second metadata call
    let mut text = String::new();
    file.take(MAX_THEME_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_THEME_BYTES {
        return Err(std::io::Error::other(format!(
            "{THEME_FILENAME} is larger than {MAX_THEME_BYTES} bytes; ignoring it"
        )));
    }
    Ok(Some(text))
}

fn scheme_block(scheme: &str, tokens: &BTreeMap<String, String>) -> String {
    format!(
        "@media (prefers-color-scheme: {scheme}) {{\n:root {{\n{}}}\n}}\n",
        declarations(tokens)
    )
}

fn declarations(tokens: &BTreeMap<String, String>) -> String {
    tokens
        .iter()
        .map(|(name, value)| format!("    --{name}: {value};\n"))
        .collect()
}

fn sanitize(tokens: BTreeMap<String, String>, block: &str) -> BTreeMap<String, String> {
    tokens
        .into_iter()
        .filter(|(name, value)| {
            if !THEMEABLE.contains(&name.as_str()) {
                eprintln!("{THEME_FILENAME}: {block}.{name} is not a themeable token; ignored");
                return false;
            }
            if !is_hex_colour(value) {
                eprintln!(
                    "{THEME_FILENAME}: {block}.{name} is not a hex colour ({value:?}); ignored"
                );
                return false;
            }
            true
        })
        .collect()
}

/// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa` — nothing else.
///
/// Deliberately narrower than CSS allows. Named colours, `rgb()`,
/// `var()` and the rest would each need their own reasoning about what
/// can hide inside them; a hex literal cannot contain a delimiter, a
/// comment, a function call or a URL, so it needs none.
fn is_hex_colour(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 4 | 6 | 8) && digits.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{Theme, is_hex_colour};

    #[test]
    fn a_theme_becomes_css_for_both_blocks() {
        let theme = Theme::parse(
            r##"{"light":{"window-bg":"#FAFAFA"},"dark":{"window-bg":"#101010","text":"#EEE"}}"##,
        );

        let css = theme.to_css();

        let light = css
            .find("@media (prefers-color-scheme: light) {")
            .expect("a light block");
        let dark = css
            .find("@media (prefers-color-scheme: dark) {")
            .expect("a dark block");
        assert!(light < dark, "light first, then dark: {css}");
        assert!(css.contains(":root {\n    --window-bg: #FAFAFA;\n}"));
        assert!(css.contains("--text: #EEE;"));
    }

    /// The bug this scoping exists to prevent. A bare `:root` would be
    /// emitted after theme.css's own dark block, win on source order at
    /// equal specificity, and paint a light-only theme over dark mode —
    /// leaving the shipped dark `--text` on the user's light background.
    #[test]
    fn a_light_only_theme_does_not_reach_dark_mode() {
        let css = Theme::parse(r##"{"light":{"window-bg":"#FFFFFF"}}"##).to_css();

        assert!(
            css.starts_with("@media (prefers-color-scheme: light) {"),
            "the light block must be scoped, not bare: {css}"
        );
        assert!(
            !css.contains("dark"),
            "nothing may apply to dark mode: {css}"
        );
    }

    /// The common case: no file, no theme, and a page identical to the one
    /// that shipped.
    #[test]
    fn an_empty_theme_emits_nothing() {
        assert_eq!(Theme::default().to_css(), "");
        assert_eq!(Theme::parse("{}").to_css(), "");
    }

    /// The attack this module exists to prevent: the value closes the
    /// declaration and opens a rule of its own inside the window that
    /// displays what the user is typing.
    #[test]
    fn a_value_that_tries_to_close_the_declaration_is_dropped() {
        let theme = Theme::parse(
            r##"{"light":{"window-bg":"red; } body { background: url(http://example.com/x) } .x {"}}"##,
        );

        assert_eq!(theme.to_css(), "");
    }

    #[test]
    fn only_hex_colours_are_values() {
        assert!(is_hex_colour("#fff"));
        assert!(is_hex_colour("#FFFF"));
        assert!(is_hex_colour("#a1b2c3"));
        assert!(is_hex_colour("#A1B2C3D4"));

        // plausible CSS that is still not allowed, because each would need
        // its own argument about what can hide inside it
        assert!(!is_hex_colour("red"));
        assert!(!is_hex_colour("rgb(1,2,3)"));
        assert!(!is_hex_colour("var(--accent)"));
        assert!(!is_hex_colour("url(http://example.com)"));
        // shapes that look like hex but are not
        assert!(!is_hex_colour("#fffff"));
        assert!(!is_hex_colour("#"));
        assert!(!is_hex_colour("#gggggg"));
        assert!(!is_hex_colour("fff"));
        assert!(!is_hex_colour(" #fff"));
    }

    /// A key the CSS does not define cannot be introduced by a theme —
    /// otherwise a theme could set any custom property it liked, and the
    /// name is the other half of what lands in the stylesheet.
    #[test]
    fn an_unknown_token_is_dropped() {
        let theme = Theme::parse(r##"{"light":{"not-a-token":"#fff","window-bg":"#000"}}"##);

        assert_eq!(
            theme.to_css(),
            "@media (prefers-color-scheme: light) {\n:root {\n    --window-bg: #000;\n}\n}\n"
        );
    }

    /// A name carrying its own punctuation is refused by the same rule,
    /// so the `--{name}` interpolation cannot be broken out of either.
    #[test]
    fn a_token_name_cannot_smuggle_syntax() {
        let theme = Theme::parse(r##"{"light":{"window-bg: red; --x":"#fff"}}"##);

        assert_eq!(theme.to_css(), "");
    }

    /// One bad entry must not cost the user the rest of their theme.
    #[test]
    fn a_bad_entry_does_not_discard_the_good_ones() {
        let theme =
            Theme::parse(r##"{"light":{"window-bg":"#000","accent":"javascript:alert(1)"}}"##);

        assert_eq!(
            theme.to_css(),
            "@media (prefers-color-scheme: light) {\n:root {\n    --window-bg: #000;\n}\n}\n"
        );
    }

    /// A hand-edited file must not stop the window from being drawn.
    #[test]
    fn malformed_json_yields_no_theme() {
        assert_eq!(Theme::parse("{ not json").to_css(), "");
        assert_eq!(Theme::parse("").to_css(), "");
    }

    /// Windows editors add one readily, and settings.json already tolerates
    /// it; a theme that silently stopped applying after an edit in Notepad
    /// would be a puzzle.
    #[test]
    fn a_utf8_bom_is_tolerated() {
        let theme = Theme::parse("\u{feff}{\"light\":{\"accent\":\"#123456\"}}");

        assert!(theme.to_css().contains("--accent: #123456;"));
    }

    /// Only one block given is a valid theme: the other keeps the shipped
    /// colours instead of being half-overridden.
    #[test]
    fn one_block_alone_is_fine() {
        let css = Theme::parse(r##"{"dark":{"window-bg":"#000"}}"##).to_css();

        assert_eq!(
            css,
            "@media (prefers-color-scheme: dark) {\n:root {\n    --window-bg: #000;\n}\n}\n"
        );
    }
}

#[cfg(test)]
mod file_tests {
    //! The paths `parse` never sees: what happens around the file.

    use super::{MAX_THEME_BYTES, THEME_FILENAME, THEMEABLE, Theme};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "azookey-theme-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create temp root");
            TempRoot(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, contents: &str) {
            std::fs::write(self.0.join(THEME_FILENAME), contents).expect("write fixture");
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read(root: &TempRoot) -> Theme {
        Theme::read_in(Some(root.path().to_path_buf()))
    }

    /// The overwhelmingly common case, and it must be silent.
    #[test]
    fn no_file_is_no_theme() {
        let root = TempRoot::new();

        assert_eq!(read(&root), Theme::default());
    }

    #[test]
    fn a_theme_file_is_read() {
        let root = TempRoot::new();
        root.write(r##"{"light":{"accent":"#123456"}}"##);

        assert!(read(&root).to_css().contains("--accent: #123456;"));
    }

    /// The bound that keeps a data-only add-on away from the core: ui.exe
    /// stuck reading this file never reaches its webview, and the
    /// supervisor that gives up on ui.exe is the one that ends the
    /// launcher.
    #[test]
    fn a_file_past_the_size_limit_is_refused() {
        let root = TempRoot::new();
        let padding = " ".repeat(MAX_THEME_BYTES as usize);
        root.write(&format!(r##"{{"light":{{"accent":"#123456"}}}}{padding}"##));

        assert_eq!(read(&root), Theme::default());
    }

    /// A file exactly at the limit is still a file.
    #[test]
    fn a_file_at_the_size_limit_is_read() {
        let root = TempRoot::new();
        let document = r##"{"light":{"accent":"#123456"}}"##;
        let padding = " ".repeat(MAX_THEME_BYTES as usize - document.len());
        root.write(&format!("{document}{padding}"));

        assert!(read(&root).to_css().contains("--accent: #123456;"));
    }

    /// Someone made a directory of that name, or a hand-edit left one.
    /// Neither may stop the window being drawn.
    #[test]
    fn a_directory_where_the_file_should_be_is_not_a_theme() {
        let root = TempRoot::new();
        std::fs::create_dir(root.path().join(THEME_FILENAME)).expect("create dir");

        assert_eq!(read(&root), Theme::default());
    }

    /// No `%APPDATA%` means no answer about where the file would be, and
    /// a root-relative guess would be worse than none.
    #[test]
    fn no_config_root_is_no_theme() {
        assert_eq!(Theme::read_in(None), Theme::default());
    }

    /// A wrong TYPE fails the document, so the whole file is discarded —
    /// unlike a wrong VALUE, which costs one entry. Pinned because the
    /// difference is serde's rather than a decision made here, and it is
    /// what the user is told in the doc comment.
    #[test]
    fn a_value_of_the_wrong_type_discards_the_whole_file() {
        let root = TempRoot::new();
        root.write(r##"{"light":{"accent":"#123456","window-bg":5}}"##);

        assert_eq!(read(&root), Theme::default(), "not even accent survives");
    }

    #[test]
    fn a_value_of_the_wrong_shape_costs_only_that_entry() {
        let root = TempRoot::new();
        root.write(r##"{"light":{"accent":"#123456","window-bg":"chartreuse"}}"##);

        let css = read(&root).to_css();
        assert!(css.contains("--accent: #123456;"));
        assert!(!css.contains("window-bg"));
    }

    /// A token theme.css defines but this module does not list cannot be
    /// themed, and one listed here that theme.css does not define is a
    /// setting with nothing behind it. Neither breaks anything, and both
    /// are the kind of drift nobody notices — so the two lists are
    /// compared rather than trusted.
    #[test]
    fn the_allow_list_matches_the_stylesheet() {
        let css = include_str!("../assets/theme.css");
        let declared: std::collections::BTreeSet<&str> = css
            .lines()
            .filter_map(|line| line.trim().strip_prefix("--"))
            .filter_map(|rest| rest.split_once(':'))
            .map(|(name, _)| name)
            .collect();
        let allowed: std::collections::BTreeSet<&str> = THEMEABLE.iter().copied().collect();

        assert_eq!(
            declared, allowed,
            "assets/theme.css and THEMEABLE have drifted apart"
        );
    }
}
