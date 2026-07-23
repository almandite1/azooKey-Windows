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

/// The icon resource id for a mode under the current theme. `light_theme` is
/// what `get_theme` reports, and it picks the icon that CONTRASTS with the
/// task bar: the black glyphs (`res/res.h`: `IDI_MODE_*_BLACK`, 102/103) are
/// for a light theme, the white ones (104/105) for a dark one. Split out of
/// `GetIcon` so the table can be tested without a module handle and a live
/// `LoadImageW`; getting it wrong shows an invisible or simply wrong mode
/// indicator.
fn langbar_icon_id(input_mode: &InputMode, light_theme: bool) -> u32 {
    match (input_mode, light_theme) {
        (InputMode::Kana, true) => 102,
        (InputMode::Latin, true) => 103,
        (InputMode::Kana, false) => 104,
        (InputMode::Latin, false) => 105,
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

        let icon_id = langbar_icon_id(&input_mode, theme);

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use windows::Win32::Foundation::E_INVALIDARG;
    use windows::Win32::System::Ole::CONNECT_E_CANNOTCONNECT;
    use windows::Win32::UI::TextServices::{
        ITfLangBarItem, ITfLangBarItemSink, ITfSource, ITfTextInputProcessor, TF_LANGBARITEMINFO,
    };
    use windows::core::{IUnknown, Interface as _};

    use super::{INFO, langbar_icon_id};
    use crate::engine::input_mode::InputMode;
    use crate::globals::TEXTSERVICE_LANGBARITEMSINK_COOKIE;
    use crate::tsf::test_support::{EditSessionBehavior, factory_with_fake_context};

    fn langbar_source() -> (ITfTextInputProcessor, ITfSource) {
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let source = tip.cast::<ITfSource>().unwrap();
        (tip, source)
    }

    /// The host connects its language-bar sink through the generic
    /// `ITfSource`, so the IID is the only thing telling us what it wants.
    #[test]
    fn the_langbar_sink_iid_is_accepted() {
        let (tip, source) = langbar_source();
        let punk: IUnknown = tip.cast().unwrap();

        let cookie = unsafe { source.AdviseSink(&ITfLangBarItemSink::IID, &punk) }.unwrap();

        assert_eq!(cookie, TEXTSERVICE_LANGBARITEMSINK_COOKIE);
    }

    /// Anything else must be refused rather than silently handed the langbar
    /// cookie — a host that advised, say, a text-edit sink would then be told
    /// it had one and never hear from us.
    #[test]
    fn another_sinks_iid_is_refused() {
        let (tip, source) = langbar_source();
        let punk: IUnknown = tip.cast().unwrap();

        let result = unsafe { source.AdviseSink(&ITfLangBarItem::IID, &punk) };

        assert_eq!(result.unwrap_err().code(), E_INVALIDARG);
    }

    /// A raw deref in a COM callback is a segfault, and `catch_unwind` cannot
    /// turn that into an HRESULT — so the null check has to be in front of it.
    #[test]
    fn a_null_iid_is_refused_instead_of_dereferenced() {
        let (tip, source) = langbar_source();
        let punk: IUnknown = tip.cast().unwrap();

        let result = unsafe { source.AdviseSink(std::ptr::null(), &punk) };

        assert_eq!(result.unwrap_err().code(), E_INVALIDARG);
    }

    #[test]
    fn a_missing_sink_object_is_refused() {
        let (_tip, source) = langbar_source();

        let result = unsafe { source.AdviseSink(&ITfLangBarItemSink::IID, None) };

        assert_eq!(result.unwrap_err().code(), E_INVALIDARG);
    }

    #[test]
    fn the_cookie_we_handed_out_can_be_unadvised() {
        let (_tip, source) = langbar_source();

        unsafe { source.UnadviseSink(TEXTSERVICE_LANGBARITEMSINK_COOKIE) }.unwrap();
    }

    /// The one cookie we ever hand out is the only one we accept back;
    /// anything else is the host confusing us with another sink.
    #[test]
    fn an_unknown_cookie_cannot_be_unadvised() {
        let (_tip, source) = langbar_source();

        let result = unsafe { source.UnadviseSink(TEXTSERVICE_LANGBARITEMSINK_COOKIE + 1) };

        assert_eq!(result.unwrap_err().code(), CONNECT_E_CANNOTCONNECT);
    }

    /// The whole theme × mode table, which is otherwise four magic numbers
    /// three call-levels deep. Black glyphs (102/103) go on the light theme.
    #[test]
    fn the_icon_matches_the_mode_and_contrasts_with_the_theme() {
        assert_eq!(langbar_icon_id(&InputMode::Kana, true), 102);
        assert_eq!(langbar_icon_id(&InputMode::Latin, true), 103);
        assert_eq!(langbar_icon_id(&InputMode::Kana, false), 104);
        assert_eq!(langbar_icon_id(&InputMode::Latin, false), 105);
    }

    /// Every combination gets its own icon: a duplicated id is the mistake
    /// this table invites, and it would make the two modes indistinguishable.
    #[test]
    fn every_mode_and_theme_pair_has_its_own_icon() {
        let ids = [
            langbar_icon_id(&InputMode::Kana, true),
            langbar_icon_id(&InputMode::Latin, true),
            langbar_icon_id(&InputMode::Kana, false),
            langbar_icon_id(&InputMode::Latin, false),
        ];

        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate icon id in {ids:?}");
        // 101 is the application icon, not a mode icon
        assert!(ids.iter().all(|id| *id > 101));
    }

    /// `GetInfo` writes through a raw pointer the host owns; a null one is a
    /// hostile (or merely broken) host, not a crash.
    #[test]
    fn get_info_fills_the_hosts_struct_and_rejects_a_null_one() {
        let (tip, _source) = langbar_source();
        let item = tip.cast::<ITfLangBarItem>().unwrap();

        let mut info = TF_LANGBARITEMINFO::default();
        unsafe { item.GetInfo(&mut info) }.unwrap();
        assert_eq!(info.guidItem, INFO.guidItem);
        assert_eq!(info.clsidService, INFO.clsidService);

        let result = unsafe { item.GetInfo(std::ptr::null_mut()) };
        assert_eq!(result.unwrap_err().code(), E_INVALIDARG);
    }

    /// Unused, but part of the interface the host may call at any time.
    #[test]
    fn the_item_reports_no_status_and_no_tooltip() {
        let (tip, _source) = langbar_source();
        let item = tip.cast::<ITfLangBarItem>().unwrap();

        assert_eq!(unsafe { item.GetStatus() }.unwrap(), 0);
        assert!(unsafe { item.GetTooltipString() }.unwrap().is_empty());
    }
}
