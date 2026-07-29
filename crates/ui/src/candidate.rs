use anyhow::Result;
use tao::{event_loop::EventLoop, window::Window};
use wry::{WebContext, WebViewBuilder};

use crate::UserEvent;
use crate::window::create_overlay_window;

pub fn create_candidate_window(event_loop: &EventLoop<UserEvent>) -> Result<Window> {
    create_overlay_window(event_loop, "CandidateList", false)
}

/// Takes the shared `WebContext` so this webview's profile lands where
/// `webview2_data_dir` decided, not next to the exe (issue #54).
/// Markup, styles, and script live in assets/ (editable as real HTML/CSS/
/// JS); theme.css carries the design tokens shared with the indicator, and
/// the user's theme follows it so the cascade does the overriding.
///
/// Takes the theme CSS rather than reading it, so a test can pin WHERE it
/// lands: a placeholder in the wrong place is a theme that silently never
/// applies, which is exactly the kind of thing that survives review.
fn candidate_html(user_theme_css: &str) -> String {
    format!(
        include_str!("../assets/candidate.html"),
        theme_css = include_str!("../assets/theme.css"),
        user_theme_css = user_theme_css,
        candidate_css = include_str!("../assets/candidate.css"),
        candidate_js = include_str!("../assets/candidate.js"),
    )
}

pub fn create_candidate_webview(context: &mut WebContext) -> Result<WebViewBuilder<'_>> {
    let html = candidate_html(&crate::theme::Theme::read().to_css());

    let webview_builder = WebViewBuilder::new_with_web_context(context)
        .with_transparent(true)
        .with_html(html);

    Ok(webview_builder)
}

#[cfg(test)]
mod tests {
    use super::candidate_html;

    /// A theme only works if its block lands INSIDE the `<style>` element
    /// and AFTER the tokens it is meant to override — CSS custom
    /// properties are resolved by cascade order, so the same declarations
    /// placed first would be silently overridden by the defaults instead.
    /// A placeholder moved to the wrong line breaks nothing visibly and
    /// nothing testably, except here.
    #[test]
    fn the_user_theme_follows_the_default_tokens_inside_the_stylesheet() {
        let html = candidate_html(":root { --window-bg: #123456; }");

        let style_open = html.find("<style>").expect("a style element");
        let style_close = html.find("</style>").expect("a closed style element");
        let defaults = html
            .find("--window-bg: #FFFFFF")
            .expect("the shipped token");
        let theme = html.find("--window-bg: #123456").expect("the user's token");

        assert!(
            style_open < defaults && defaults < theme && theme < style_close,
            "the theme must sit after the defaults and inside the stylesheet"
        );
    }

    /// The overwhelmingly common case — no theme file at all — must leave
    /// the page exactly as it shipped rather than with a stray blank rule.
    #[test]
    fn no_theme_leaves_no_trace() {
        let html = candidate_html("");

        assert!(!html.contains(":root {\n}"), "no empty rule may be emitted");
        assert!(
            html.contains("--window-bg: #FFFFFF"),
            "defaults still there"
        );
    }
}
