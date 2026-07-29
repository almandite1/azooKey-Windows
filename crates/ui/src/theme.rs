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
        let Some(path) = shared::config_root().map(|dir| dir.join(THEME_FILENAME)) else {
            return Theme::default();
        };
        if !path.exists() {
            return Theme::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => Theme::parse(&text),
            Err(e) => {
                eprintln!("failed to read {}: {e}", path.display());
                Theme::default()
            }
        }
    }

    /// Parses a theme document and drops everything that is not a known
    /// token set to a hex colour.
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
    /// Empty when there is nothing to say, which keeps the generated page
    /// byte-identical to the untouched one on the overwhelmingly common
    /// path of no theme at all.
    pub fn to_css(&self) -> String {
        let mut css = String::new();
        if !self.light.is_empty() {
            css.push_str(&format!(":root {{\n{}}}\n", declarations(&self.light)));
        }
        if !self.dark.is_empty() {
            css.push_str(&format!(
                "@media (prefers-color-scheme: dark) {{\n:root {{\n{}}}\n}}\n",
                declarations(&self.dark)
            ));
        }
        css
    }
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

        assert!(css.contains(":root {\n    --window-bg: #FAFAFA;\n}"));
        assert!(css.contains("@media (prefers-color-scheme: dark) {"));
        assert!(css.contains("--text: #EEE;"));
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

        assert_eq!(theme.to_css(), ":root {\n    --window-bg: #000;\n}\n");
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

        assert_eq!(theme.to_css(), ":root {\n    --window-bg: #000;\n}\n");
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
        let dark_only = Theme::parse(r##"{"dark":{"window-bg":"#000"}}"##);

        let css = dark_only.to_css();
        assert!(css.starts_with("@media (prefers-color-scheme: dark)"));
        assert!(!css.contains(":root {\n    --window-bg: #000;\n}\n:root"));
    }
}
