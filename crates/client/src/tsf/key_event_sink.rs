use windows::{
    Win32::{
        Foundation::{BOOL, LPARAM, WPARAM},
        UI::TextServices::{ITfContext, ITfKeyEventSink_Impl},
    },
    core::GUID,
};

use anyhow::Result;

use super::factory::TextServiceFactory_Impl;

// sink (aka event listener) for key events
impl ITfKeyEventSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    // skip(self, pic): TextServiceFactory_Impl has no Debug, and the context
    // pointer says nothing a reader can act on. wparam — which key — is the
    // field that makes these spans worth having.
    #[tracing::instrument(skip(self, pic))]
    fn OnTestKeyDown(
        &self,
        pic: Option<&ITfContext>,
        wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        // TRUE = we will handle this key in OnKeyDown. A pure query, as the
        // contract requires (issue #26): a Ctrl/Alt chord during a
        // composition answers TRUE, and the cancel runs in OnKeyDown when
        // the host delivers the key — never here, so a speculative probe
        // cannot discard the user's composition.
        let result = self.test_key(pic, wparam)?;

        Ok(result.into())
    }

    #[macros::anyhow]
    #[tracing::instrument(skip(self, pic))]
    fn OnKeyDown(&self, pic: Option<&ITfContext>, wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
        // this function is called when a key is pressed
        // we can handle key events here
        let result = self.handle_key(pic, wparam)?;

        Ok(result.into())
    }

    #[macros::anyhow]
    fn OnTestKeyUp(
        &self,
        _pic: Option<&ITfContext>,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        // same as OnTestKeyDown
        Ok(false.into())
    }

    #[macros::anyhow]
    fn OnKeyUp(&self, _pic: Option<&ITfContext>, _wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
        // this function is called when a key is released
        // but we handle key events in OnKeyDown function
        // so just return S_OK
        Ok(false.into())
    }

    #[macros::anyhow]
    fn OnPreservedKey(&self, _pic: Option<&ITfContext>, _rguid: *const GUID) -> Result<BOOL> {
        // this function is actually not used
        Ok(true.into())
    }

    #[macros::anyhow]
    fn OnSetFocus(&self, fforeground: BOOL) -> Result<()> {
        // Gaining the foreground is where the mode can have been changed
        // behind our back — by the touch keyboard, the shell, or an IMM32
        // app while another window had focus.
        if !fforeground.as_bool() {
            return Ok(());
        }

        // advisory: never break typing over a mode read (see CLAUDE.md)
        if let Err(error) = self.sync_input_mode_from_compartments() {
            tracing::warn!("OnSetFocus: compartment sync failed: {error:?}");
        }

        Ok(())
    }
}
