use anyhow::{Context as _, Result};
use tao::{dpi::LogicalSize, event_loop::EventLoop, window::Window};
use wry::{WebView, WebViewBuilder};

use crate::window::create_overlay_window;
use crate::UserEvent;

pub fn create_indicator_window(event_loop: &EventLoop<UserEvent>) -> Result<Window> {
    let window = create_overlay_window(event_loop, "Indicator", true)?;

    // logical px: the webview lays out in CSS px, so a physical 90x90 was
    // too small at high DPI and the mode glyph overflowed (B20)
    window.set_inner_size(LogicalSize::new(90.0, 90.0));

    Ok(window)
}

pub fn create_indicator_webview(window: &Window) -> Result<WebView> {
    // Shares theme.css (the design tokens) with the candidate window; the
    // accent border is the indicator's own look (assets/indicator.css).
    let html = format!(
        include_str!("../assets/indicator.html"),
        theme_css = include_str!("../assets/theme.css"),
        indicator_css = include_str!("../assets/indicator.css"),
        indicator_js = include_str!("../assets/indicator.js"),
    );

    let webview = WebViewBuilder::new()
        .with_transparent(true)
        .with_html(html)
        .build(window)
        .context("Failed to create webview")?;

    Ok(webview)
}
