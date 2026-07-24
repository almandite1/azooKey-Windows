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

    /// The value of one specific control, addressed by the AutomationId that
    /// UI Automation derives from a Win32 control's id.
    ///
    /// `text_of` returns whichever element happens to carry text first, which
    /// is fine when a host has one field and useless when it has three. This
    /// is how the password scenario reads the mirror field and nothing else.
    pub fn value_by_automation_id(&self, window: HWND, id: &str) -> Option<String> {
        let elements = self.descendants(window)?;
        elements.into_iter().find_map(|element| {
            let matches = unsafe { element.CurrentAutomationId() }.is_ok_and(|found| found == id);
            matches.then(|| value_of(&element).unwrap_or_default())
        })
    }

    /// A one-line description of every control in the window: its
    /// AutomationId, class, whether the tree marks it a password, and its
    /// value.
    ///
    /// Diagnostics, not an assertion. The password scenario kept failing in
    /// ways that could equally have meant "the IME did not disengage", "Tab
    /// never moved", or "the readback is wrong", and no amount of rephrasing
    /// the assertion separated them — the harness has to be able to say what
    /// it is actually looking at.
    pub fn describe(&self, window: HWND) -> Vec<String> {
        let Some(elements) = self.descendants(window) else {
            return vec!["<no UIA tree for this window>".to_string()];
        };
        elements
            .into_iter()
            .map(|element| {
                let id = unsafe { element.CurrentAutomationId() }
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let class = unsafe { element.CurrentClassName() }
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let password = unsafe { element.CurrentIsPassword() }
                    .map(|b| b.as_bool())
                    .unwrap_or(false);
                let value = value_of(&element).unwrap_or_else(|| "<no value pattern>".to_string());
                format!("id={id:?} class={class:?} password={password} value={value:?}")
            })
            .collect()
    }

    /// The AutomationId of whatever currently has focus, for telling "Tab
    /// moved" from "Tab did nothing".
    pub fn focused_automation_id(&self) -> Option<String> {
        unsafe {
            let element = self.automation.GetFocusedElement().ok()?;
            Some(element.CurrentAutomationId().ok()?.to_string())
        }
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
