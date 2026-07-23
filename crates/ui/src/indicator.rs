use anyhow::{Context as _, Result};
use tao::{dpi::LogicalSize, event_loop::EventLoop, window::Window};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::UserEvent;
use crate::window::create_overlay_window;

pub fn create_indicator_window(event_loop: &EventLoop<UserEvent>) -> Result<Window> {
    let window = create_overlay_window(event_loop, "Indicator", true)?;

    // logical px: the webview lays out in CSS px, so a physical 90x90 was
    // too small at high DPI and the mode glyph overflowed (B20)
    window.set_inner_size(LogicalSize::new(90.0, 90.0));

    Ok(window)
}

/// Shares the candidate window's `WebContext`, so both webviews live in the
/// one profile under `%LOCALAPPDATA%` (issue #54). Sharing it is also what
/// keeps their `CoreWebView2EnvironmentOptions` identical, which WebView2
/// requires of two environments over the same user data folder.
pub fn create_indicator_webview<'a>(
    window: &'a Window,
    context: &'a mut WebContext,
) -> Result<WebView> {
    // Shares theme.css (the design tokens) with the candidate window; the
    // accent border is the indicator's own look (assets/indicator.css).
    let html = format!(
        include_str!("../assets/indicator.html"),
        theme_css = include_str!("../assets/theme.css"),
        indicator_css = include_str!("../assets/indicator.css"),
        indicator_js = include_str!("../assets/indicator.js"),
    );

    let webview = WebViewBuilder::new_with_web_context(context)
        .with_transparent(true)
        .with_html(html)
        .build(window)
        .context("Failed to create webview")?;

    Ok(webview)
}
