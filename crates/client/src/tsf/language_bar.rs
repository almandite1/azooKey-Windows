use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, HINSTANCE, POINT, RECT},
        System::Ole::CONNECT_E_CANNOTCONNECT,
        UI::{
            TextServices::{
                GUID_LBI_INPUTMODE, ITfLangBarItem_Impl, ITfLangBarItemButton,
                ITfLangBarItemButton_Impl, ITfLangBarItemMgr, ITfLangBarItemSink, ITfMenu,
                ITfSource_Impl, ITfThreadMgr, TF_LANGBARITEMINFO, TF_LBI_STYLE_BTN_BUTTON,
                TfLBIClick,
            },
            WindowsAndMessaging::{HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW},
        },
    },
    core::{BOOL, BSTR, GUID, IUnknown, Interface as _, PCWSTR},
};

use crate::{
    engine::{
        client_action::ClientAction, composition::CompositionState, input_mode::InputMode,
        theme::get_theme,
    },
    globals::{DllModule, GUID_TEXT_SERVICE, TEXTSERVICE_LANGBARITEMSINK_COOKIE},
};

use anyhow::{Context as _, Result};

use super::factory::TextServiceFactory_Impl;

impl TextServiceFactory_Impl {
    /// Adds our mode button to the host's language bar. Extracted so the
    /// three sites that touch the langbar item (Activate, Deactivate via
    /// `remove_langbar_item`, and `update_lang_bar`'s remove-then-add icon
    /// refresh) share one AddItem/RemoveItem pair instead of open-coding the
    /// `cast::<ITfLangBarItemMgr>()` + `this::<ITfLangBarItemButton>()` dance.
    pub fn add_langbar_item(&self, thread_mgr: &ITfThreadMgr) -> Result<()> {
        unsafe {
            thread_mgr
                .cast::<ITfLangBarItemMgr>()?
                .AddItem(&self.this::<ITfLangBarItemButton>()?)?;
        }
        Ok(())
    }

    /// Removes our mode button from the host's language bar.
    pub fn remove_langbar_item(&self, thread_mgr: &ITfThreadMgr) -> Result<()> {
        unsafe {
            thread_mgr
                .cast::<ITfLangBarItemMgr>()?
                .RemoveItem(&self.this::<ITfLangBarItemButton>()?)?;
        }
        Ok(())
    }
}

const INFO: TF_LANGBARITEMINFO = TF_LANGBARITEMINFO {
    clsidService: GUID_TEXT_SERVICE,
    guidItem: GUID_LBI_INPUTMODE,
    dwStyle: TF_LBI_STYLE_BTN_BUTTON,
    ulSort: 0,
    szDescription: [0; 32],
};

// you need to implement these three interfaces to create a language bar item
// if not, you will get E_FAIL error in ITfLangBarItemMgr::AddItem

impl ITfLangBarItem_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn GetInfo(&self, p_info: *mut TF_LANGBARITEMINFO) -> Result<()> {
        // a raw deref in a COM callback is a segfault, which catch_unwind
        // cannot turn into an HRESULT (same guard as GetPageIndex)
        if p_info.is_null() {
            return Err(windows_core::Error::from_hresult(E_INVALIDARG).into());
        }
        unsafe {
            *p_info = INFO;
        }
        Ok(())
    }

    #[macros::anyhow]
    fn GetStatus(&self) -> Result<u32> {
        Ok(0)
    }

    #[macros::anyhow]
    fn Show(&self, _f_show: BOOL) -> Result<()> {
        Ok(())
    }

    // this will be shown as a tooltip when you hover the language bar item
    #[macros::anyhow]
    fn GetTooltipString(&self) -> Result<BSTR> {
        Ok(BSTR::default())
    }
}

impl ITfLangBarItemButton_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnClick(&self, _click: TfLBIClick, _pt: &POINT, _prcarea: *const RECT) -> Result<()> {
        let mode = {
            match self.borrow()?.input_mode {
                InputMode::Latin => InputMode::Kana,
                InputMode::Kana => InputMode::Latin,
            }
        };

        let actions = vec![ClientAction::SetIMEMode(mode)];
        self.handle_action(&actions, CompositionState::None)?;

        Ok(())
    }

    // this method should not be called
    #[macros::anyhow]
    fn InitMenu(&self, _pmenu: windows_core::Ref<'_, ITfMenu>) -> Result<()> {
        Ok(())
    }

    // this method should not be called
    #[macros::anyhow]
    fn OnMenuSelect(&self, _w_id: u32) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn GetIcon(&self) -> Result<HICON> {
        let dll_module = DllModule::get()?;
        let input_mode = self.borrow()?.input_mode.clone();
        let theme = get_theme()?;

        let icon_id = match input_mode {
            InputMode::Kana => {
                if theme {
                    102
                } else {
                    104
                }
            }
            InputMode::Latin => {
                if theme {
                    103
                } else {
                    105
                }
            }
        };

        unsafe {
            let handle = LoadImageW(
                // 0.62 takes an Option<HINSTANCE> here rather than the HMODULE
                // the DLL entry point handed us; same handle either way
                Some(HINSTANCE(
                    dll_module.hinst.context("Dll instance not found")?.0,
                )),
                PCWSTR(icon_id as *mut u16),
                IMAGE_ICON,
                0,
                0,
                LR_DEFAULTCOLOR,
            )?;

            Ok(HICON(handle.0))
        }
    }

    #[macros::anyhow]
    fn GetText(&self) -> Result<BSTR> {
        Ok(BSTR::default())
    }
}

impl ITfSource_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn AdviseSink(&self, riid: *const GUID, punk: windows_core::Ref<'_, IUnknown>) -> Result<u32> {
        // a raw deref in a COM callback is a segfault, which catch_unwind
        // cannot turn into an HRESULT (same guard as GetPageIndex)
        if riid.is_null() {
            return Err(windows_core::Error::from_hresult(E_INVALIDARG).into());
        }
        let riid = unsafe { *riid };

        if riid != ITfLangBarItemSink::IID {
            return Err(anyhow::Error::new(windows_core::Error::from_hresult(
                E_INVALIDARG,
            )));
        }

        if punk.is_none() {
            return Err(anyhow::Error::new(windows_core::Error::from_hresult(
                E_INVALIDARG,
            )));
        }

        Ok(TEXTSERVICE_LANGBARITEMSINK_COOKIE)
    }

    #[macros::anyhow]
    fn UnadviseSink(&self, dw_cookie: u32) -> Result<()> {
        if dw_cookie != TEXTSERVICE_LANGBARITEMSINK_COOKIE {
            return Err(anyhow::Error::new(windows_core::Error::from_hresult(
                CONNECT_E_CANNOTCONNECT,
            )));
        }

        Ok(())
    }
}
