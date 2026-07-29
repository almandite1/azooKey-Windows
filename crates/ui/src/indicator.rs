use anyhow::{Context as _, Result};
use tao::{dpi::LogicalSize, event_loop::EventLoop, window::Window};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::UserEvent;
use crate::window::create_overlay_window;

/// Side of the (square) mode indicator, in CSS px: big enough for the あ/A
/// glyph and its badge, small enough to sit beside the caret without
/// covering the text.
const INDICATOR_SIDE: f64 = 90.0;

pub fn create_indicator_window(event_loop: &EventLoop<UserEvent>) -> Result<Window> {
    let window = create_overlay_window(event_loop, "Indicator", true)?;

    // logical px: the webview lays out in CSS px, so a physical 90x90 was
    // too small at high DPI and the mode glyph overflowed (B20)
    window.set_inner_size(LogicalSize::new(INDICATOR_SIDE, INDICATOR_SIDE));

    Ok(window)
}

/// Shares theme.css (the design tokens) with the candidate window; the
/// accent border is the indicator's own look (assets/indicator.css). Takes
/// the theme CSS rather than reading it for the same reason as
/// [`crate::candidate`]'s builder.
fn indicator_html(user_theme_css: &str) -> String {
    format!(
        include_str!("../assets/indicator.html"),
        theme_css = include_str!("../assets/theme.css"),
        user_theme_css = user_theme_css,
        indicator_css = include_str!("../assets/indicator.css"),
        indicator_js = include_str!("../assets/indicator.js"),
    )
}

/// Shares the candidate window's `WebContext`, so both webviews live in the
/// one profile under `%LOCALAPPDATA%` (issue #54). Sharing it is also what
/// keeps their `CoreWebView2EnvironmentOptions` identical, which WebView2
/// requires of two environments over the same user data folder.
pub fn create_indicator_webview<'a>(
    window: &'a Window,
    context: &'a mut WebContext,
) -> Result<WebView> {
    let html = indicator_html(&crate::theme::Theme::read().to_css());

    let webview = WebViewBuilder::new_with_web_context(context)
        .with_transparent(true)
        .with_html(html)
        .build(window)
        .context("Failed to create webview")?;

    Ok(webview)
}

#[cfg(test)]
mod tests {
    use super::indicator_html;

    /// The indicator shares theme.css, so it has to share the theme too —
    /// an あ/A badge that kept the shipped colours while the candidate
    /// window changed would look like a bug in the theme.
    #[test]
    fn the_indicator_takes_the_same_theme() {
        let html = indicator_html(":root { --accent: #123456; }");

        let defaults = html.find("--accent: #2CB5FF").expect("the shipped token");
        let theme = html.find("--accent: #123456").expect("the user's token");

        assert!(defaults < theme, "the theme must come after the defaults");
        assert!(theme < html.find("</style>").expect("a closed style element"));
    }
}
