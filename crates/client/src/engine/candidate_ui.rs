//! Publishing the candidate list, and opening/closing the surface it is
//! drawn on.
//!
//! Two consumers, always kept in step: the host's UILess element
//! (`tsf/ui_element.rs`) and our own `ui.exe` window. Everything here is
//! advisory — a host-side element problem must never stop the candidates
//! reaching our own window, and must never break typing (CLAUDE.md).

use anyhow::Result;
use windows::Win32::UI::TextServices::{
    TF_CLUIE_COUNT, TF_CLUIE_CURRENTPAGE, TF_CLUIE_PAGEINDEX, TF_CLUIE_SELECTION, TF_CLUIE_STRING,
};

use crate::tsf::factory::TextServiceFactory_Impl;

use super::{
    composition::CompositionEdit,
    ipc_service::{Candidates, IPCService},
};

/// Flags for `ui_update` when the whole candidate list was replaced.
pub(super) const CANDIDATES_CHANGED: u32 = TF_CLUIE_COUNT
    | TF_CLUIE_STRING
    | TF_CLUIE_SELECTION
    | TF_CLUIE_CURRENTPAGE
    | TF_CLUIE_PAGEINDEX;

/// Flags for `ui_update` when only the highlighted candidate moved.
pub(super) const SELECTION_CHANGED: u32 = TF_CLUIE_SELECTION | TF_CLUIE_CURRENTPAGE;

impl TextServiceFactory_Impl {
    /// Hands the candidate list to the host (UILess mode) and, unless the
    /// host said it draws them itself, to our own window.
    ///
    /// Every candidate update goes through here so the two can never
    /// disagree about what is displayed.
    ///
    /// Infallible by construction, and typed that way: the UILess half is
    /// advisory and the window RPCs swallow their own failures, so there was
    /// never an `Err` for a caller's `?` to carry.
    pub(super) fn publish_candidates(
        &self,
        ipc_service: &IPCService,
        candidates: &Candidates,
        selection_index: i32,
        updated_flags: u32,
    ) {
        // Advisory (CLAUDE.md): UILess bookkeeping must never break typing.
        // Propagating here would mean a host-side element problem also
        // stopped the candidates reaching our own window.
        if let Err(error) = self.ui_update(candidates, selection_index, updated_flags) {
            tracing::warn!("ui_update failed (non-fatal): {error:?}");
        }

        if self.ui_should_show() {
            if updated_flags & TF_CLUIE_STRING != 0 {
                ipc_service.set_candidates(candidates.texts.clone());
            }
            ipc_service.set_selection(selection_index);
        }
    }

    /// Renders the adopted preview into the document and republishes the whole
    /// candidate list. The tail shared by append and remove; shrink commits
    /// with `shift_start` instead of `set_text`, so it publishes on its own.
    ///
    /// Fallible only through `set_text`, which edits the document — the
    /// publishing half cannot fail.
    pub(super) fn render_and_publish_full(
        &self,
        edit: &CompositionEdit,
        ipc_service: &IPCService,
    ) -> Result<()> {
        self.set_text(&edit.preview, &edit.suffix)?;
        self.publish_candidates(
            ipc_service,
            &edit.candidates,
            edit.selection_index,
            CANDIDATES_CHANGED,
        );
        Ok(())
    }

    /// Opens the candidate UI for a new composition: asks the host first
    /// (UILess), and shows our own window only when the host does not draw
    /// the candidates itself. Advisory throughout — a host-side element
    /// problem must not break typing, so on failure we fall back to our own
    /// window (the pre-UILess behaviour).
    ///
    /// Counterpart of `close_candidate_ui`; the visibility transitions live
    /// in this pair (and host-driven `ITfUIElement::Show`) only, so an arm
    /// cannot forget one half of the teardown again (issue #21).
    pub(super) fn open_candidate_ui(&self, ipc_service: &IPCService) {
        let show = self.ui_begin().unwrap_or_else(|error| {
            tracing::warn!("ui_begin failed (non-fatal): {error:?}");
            true
        });
        if show {
            ipc_service.show_window();
        }
    }

    /// Closes the candidate UI: releases the host's UI element (UILess) and
    /// hides our own window, blanking the now-stale list. Everything here is
    /// unconditional and advisory: hiding is safe even if we never showed,
    /// and ui_end is a no-op with no live element.
    pub(super) fn close_candidate_ui(&self, ipc_service: &IPCService) {
        if let Err(error) = self.ui_end() {
            tracing::warn!("ui_end failed (non-fatal): {error:?}");
        }
        ipc_service.hide_window();
        ipc_service.set_candidates(vec![]);
    }
}
