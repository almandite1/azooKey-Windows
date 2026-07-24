//! Reading the host application's text back out.
//!
//! UI Automation rather than `WM_GETTEXT`: Windows 11's Notepad is a packaged
//! application whose editor is not a plain EDIT control, and the same code has
//! to work against whatever second host the suite grows (the plan's own
//! `ES_PASSWORD` host included). Both patterns are tried because Edit controls
//! expose Value and document surfaces expose Text.

use anyhow::{Context as _, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationValuePattern, TreeScope_Descendants, UIA_TextPatternId, UIA_ValuePatternId,
};

pub struct Uia {
    automation: IUIAutomation,
}

impl Uia {
    pub fn new() -> Result<Self> {
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL) }
                .context("failed to create the UI Automation client")?;
        Ok(Self { automation })
    }

    /// The text content of `window`, as an assistive technology would read it.
    ///
    /// Returns the first non-empty value found anywhere in the tree: the
    /// editor is the only element in these hosts that carries text, and
    /// hunting for it by control type would have to know each host's shape.
    pub fn text_of(&self, window: HWND) -> Option<String> {
        let elements = self.descendants(window)?;

        for element in elements {
            if let Some(text) = value_of(&element).or_else(|| document_text_of(&element))
                && !text.trim().is_empty()
            {
                return Some(text);
            }
        }
        None
    }

    fn descendants(&self, window: HWND) -> Option<Vec<IUIAutomationElement>> {
        unsafe {
            let root = self.automation.ElementFromHandle(window).ok()?;
            let condition = self.automation.CreateTrueCondition().ok()?;
            let found = root.FindAll(TreeScope_Descendants, &condition).ok()?;
            let count = found.Length().ok()?;
            Some(
                (0..count)
                    .filter_map(|i| found.GetElement(i).ok())
                    .collect(),
            )
        }
    }
}

fn value_of(element: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let pattern = element
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()?;
        Some(pattern.CurrentValue().ok()?.to_string())
    }
}

fn document_text_of(element: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let pattern = element
            .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            .ok()?;
        let range = pattern.DocumentRange().ok()?;
        // -1: no length limit
        Some(range.GetText(-1).ok()?.to_string())
    }
}
