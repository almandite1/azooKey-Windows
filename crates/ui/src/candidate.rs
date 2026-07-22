use anyhow::Result;
use tao::{event_loop::EventLoop, window::Window};
use wry::{WebContext, WebViewBuilder};

use crate::window::create_overlay_window;
use crate::UserEvent;

pub fn create_candidate_window(event_loop: &EventLoop<UserEvent>) -> Result<Window> {
    create_overlay_window(event_loop, "CandidateList", false)
}

/// Takes the shared `WebContext` so this webview's profile lands where
/// `webview2_data_dir` decided, not next to the exe (issue #54).
pub fn create_candidate_webview(context: &mut WebContext) -> Result<WebViewBuilder<'_>> {
    // Markup, styles, and script live in assets/ (editable as real HTML/CSS/
    // JS); theme.css carries the design tokens shared with the indicator.
    let html = format!(
        include_str!("../assets/candidate.html"),
        theme_css = include_str!("../assets/theme.css"),
        candidate_css = include_str!("../assets/candidate.css"),
        candidate_js = include_str!("../assets/candidate.js"),
    );

    let webview_builder = WebViewBuilder::new_with_web_context(context)
        .with_transparent(true)
        .with_html(html);

    Ok(webview_builder)
}
