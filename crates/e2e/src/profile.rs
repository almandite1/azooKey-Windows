//! Making azooKey the default Japanese input method, and putting the previous
//! one back afterwards.
//!
//! This is the mechanism the plan called its biggest technical risk. The
//! manual Phase 0 spike confirmed the *shape* of it: a profile set as the
//! ja-JP default is picked up by applications started AFTER the change, and
//! not by ones already running. So every scenario launches its own host
//! application rather than reusing one.

use anyhow::{Context as _, Result};
use windows::Win32::Globalization::LocaleNameToLCID;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::TextServices::{
    CLSID_TF_InputProcessorProfiles, GUID_TFCAT_TIP_KEYBOARD, ITfInputProcessorProfiles,
};
use windows::core::{GUID, w};

/// azooKey's text service and profile, from `crates/client/src/globals.rs`.
/// Duplicated rather than shared: this crate must not link the TIP DLL, whose
/// `DllMain` belongs inside host applications, not here.
const GUID_TEXT_SERVICE: GUID = GUID::from_u128(0xffdefe79_2fc2_11ef_b16b_94e70b2c378c);
const GUID_PROFILE: GUID = GUID::from_u128(0xffdefe7a_2fc2_11ef_b16b_94e70b2c378c);

/// Sets azooKey as the ja-JP default and restores the previous default on
/// drop. A run that panics or fails half way must not leave the VM's default
/// input method switched — even though the VM is restored from a checkpoint
/// anyway, several scenarios share one boot.
pub struct DefaultProfile {
    profiles: ITfInputProcessorProfiles,
    langid: u16,
    previous: Option<(GUID, GUID)>,
}

impl DefaultProfile {
    /// Switches the ja-JP default to azooKey.
    ///
    /// # Safety of the caller's environment
    /// This changes a per-user system setting. `guard::require_vm` must have
    /// passed first.
    pub fn set_to_azookey() -> Result<Self> {
        unsafe {
            let profiles: ITfInputProcessorProfiles =
                CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)
                    .context("failed to create the TSF profile manager")?;

            // the language id azooKey registers itself under (register.rs)
            let langid: u16 = LocaleNameToLCID(w!("ja-JP"), 0)
                .try_into()
                .context("ja-JP resolved to an out-of-range LCID")?;

            // remember what was default so the VM is left as it was found
            let mut clsid = GUID::zeroed();
            let mut profile = GUID::zeroed();
            let previous = profiles
                .GetDefaultLanguageProfile(
                    langid,
                    &GUID_TFCAT_TIP_KEYBOARD,
                    &mut clsid,
                    &mut profile,
                )
                .ok()
                .map(|()| (clsid, profile));
            match previous {
                Some((clsid, _)) => println!("previous default: {clsid:?}"),
                None => println!("previous default: (none reported)"),
            }

            profiles
                .SetDefaultLanguageProfile(langid, &GUID_TEXT_SERVICE, &GUID_PROFILE)
                .context(
                    "SetDefaultLanguageProfile failed — is the TIP registered (regsvr32) in this VM?",
                )?;
            println!("default set to azooKey ({GUID_TEXT_SERVICE:?})");

            Ok(Self {
                profiles,
                langid,
                previous,
            })
        }
    }

    /// Whether there was another input method to cycle through.
    pub fn has_previous(&self) -> bool {
        self.previous.is_some()
    }

    /// Switches the ja-JP default away to whatever was default before, then
    /// back to azooKey — one round trip of the checklist's "MS-IME ↔ azooKey
    /// を往復". Applications started afterwards pick azooKey up again; a TIP
    /// that did not survive being deactivated would fail the conversion that
    /// follows.
    pub fn cycle(&self) -> Result<()> {
        let Some((clsid, profile)) = self.previous else {
            return Ok(());
        };
        unsafe {
            self.profiles
                .SetDefaultLanguageProfile(self.langid, &clsid, &profile)
                .context("failed to switch the default away from azooKey")?;
            // let the switch settle before switching back; a profile change is
            // broadcast, not instantaneous
            std::thread::sleep(std::time::Duration::from_millis(500));
            self.profiles
                .SetDefaultLanguageProfile(self.langid, &GUID_TEXT_SERVICE, &GUID_PROFILE)
                .context("failed to switch the default back to azooKey")?;
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Ok(())
    }
}

impl Drop for DefaultProfile {
    fn drop(&mut self) {
        let Some((clsid, profile)) = self.previous else {
            return;
        };
        // best effort: the run is already over, and the VM is restored from a
        // checkpoint before the next one regardless
        let restored = unsafe {
            self.profiles
                .SetDefaultLanguageProfile(self.langid, &clsid, &profile)
        };
        match restored {
            Ok(()) => println!("default input method restored"),
            Err(e) => eprintln!("failed to restore the default input method: {e}"),
        }
    }
}
