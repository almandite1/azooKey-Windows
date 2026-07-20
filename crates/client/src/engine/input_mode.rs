use crate::{engine::state::IMEState, tsf::factory::TextServiceFactory};

use anyhow::Result;

#[derive(Default, Clone, PartialEq, Debug)]
pub enum InputMode {
    #[default]
    Latin,
    Kana,
}

impl TextServiceFactory {
    /// Applies `mode` everywhere it is visible: the cached field, the
    /// language bar, and the mode indicator in ui.exe.
    ///
    /// Split out of `ClientAction::SetIMEMode` so a compartment change can
    /// reuse it. `OnChange` must **not** go through the `SetIMEMode` arm:
    /// that arm runs `start_composition` / `end_composition` / `clear_text`
    /// and takes `IMEState::get()`, which from inside a TSF callback trips
    /// the re-entrancy bail in `engine/state.rs` and edits a document the
    /// host is halfway through reconfiguring.
    ///
    /// `write_compartments` is false when the change *came from* the
    /// compartments — publishing it straight back would be an echo.
    pub fn apply_input_mode(&self, mode: InputMode, write_compartments: bool) -> Result<()> {
        // scope the borrow: update_lang_bar re-enters the TextService RefCell
        // through AddItem -> GetIcon
        {
            self.borrow_mut()?.input_mode = mode.clone();
        }

        self.update_lang_bar()?;

        if let Some(mut ipc_service) = IMEState::get()?.ipc_service.clone() {
            ipc_service.set_input_mode(match mode {
                InputMode::Latin => "A",
                InputMode::Kana => "あ",
            });
        }

        if write_compartments {
            self.write_compartments(&mode)?;
        }

        Ok(())
    }

    pub fn update_lang_bar(&self) -> Result<()> {
        // refresh the icon by removing and re-adding the langbar item
        let thread_mgr = self.borrow()?.thread_mgr()?;
        self.remove_langbar_item(&thread_mgr)?;
        self.add_langbar_item(&thread_mgr)?;
        Ok(())
    }
}
