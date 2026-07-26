//! The UI Automation client both harnesses build on.
//!
//! Only the base is shared — creating the client and flattening a window's
//! element tree. What each harness asks the tree is legitimately different
//! (the display tests look for list items and selection state; the E2E suite
//! reads a host application's text back), so those queries stay where they
//! are.

use anyhow::{Context as _, Result};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Descendants,
};

use crate::Hwnd;

/// Which COM apartment the client is created in.
///
/// Made explicit because the two harnesses genuinely differ and the
/// difference is invisible at the call site: the display tests run on a
/// thread that pumps no messages and initialise an MTA of their own, while
/// the E2E harness runs on `main`'s existing STA. Getting this wrong does not
/// fail loudly — it produces a client that appears to work and then blocks or
/// returns nothing.
#[derive(Clone, Copy, Debug)]
pub enum Apartment {
    /// Initialise a multithreaded apartment on this thread. For a thread with
    /// no message loop.
    InitMultiThreaded,
    /// Use whatever apartment the caller has already entered.
    Inherit,
}

/// A UI Automation client, plus the one query both harnesses share.
pub struct UiaBase {
    automation: IUIAutomation,
}

impl UiaBase {
    pub fn new(apartment: Apartment) -> Result<Self> {
        unsafe {
            if let Apartment::InitMultiThreaded = apartment {
                // already-initialised is not an error here: several harness
                // objects can be built on the same thread
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL)
                .context("failed to create the UI Automation client")?;
            Ok(Self { automation })
        }
    }

    /// The raw client, for the queries that stay with each harness.
    pub fn automation(&self) -> &IUIAutomation {
        &self.automation
    }

    /// Every element in `window`'s tree, flattened.
    ///
    /// Empty when the window has no UIA tree at all (it is gone, or it never
    /// exposed one). Both harnesses treat that the same way — as "nothing
    /// found" rather than as an error — because at this point they have
    /// already established the window exists.
    pub fn descendants(&self, window: Hwnd) -> Vec<IUIAutomationElement> {
        unsafe {
            let Ok(root) = self.automation.ElementFromHandle(window.raw()) else {
                return Vec::new();
            };
            let Ok(condition) = self.automation.CreateTrueCondition() else {
                return Vec::new();
            };
            let Ok(found) = root.FindAll(TreeScope_Descendants, &condition) else {
                return Vec::new();
            };
            let count = found.Length().unwrap_or(0);
            (0..count)
                .filter_map(|i| found.GetElement(i).ok())
                .collect()
        }
    }
}
