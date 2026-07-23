use windows::{
    Win32::{
        Globalization::LocaleNameToLCID,
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            Registry::HKEY_CLASSES_ROOT,
        },
        UI::{
            Input::KeyboardAndMouse::HKL,
            TextServices::{
                CLSID_TF_CategoryMgr, CLSID_TF_InputProcessorProfiles,
                GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER, GUID_TFCAT_TIP_KEYBOARD,
                GUID_TFCAT_TIPCAP_COMLESS, GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
                GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT, GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
                GUID_TFCAT_TIPCAP_UIELEMENTENABLED, ITfCategoryMgr, ITfInputProcessorProfileMgr,
            },
        },
    },
    core::{GUID, w},
};

use crate::{
    extension::{GUIDExt as _, RegKey as _, StringExt as _},
    globals::{CLSID_PREFIX, GUID_PROFILE, GUID_TEXT_SERVICE, INPROC_SUFFIX, SERVICE_NAME},
};

// register the information (lang, icon, name, path_of_dll) of the TextService
pub struct ProfileMgr;

impl ProfileMgr {
    pub fn register(dll_path: &str) -> anyhow::Result<()> {
        unsafe {
            let profiles: ITfInputProcessorProfileMgr =
                CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;

            let langid: u16 = LocaleNameToLCID(w!("ja-JP"), 0).try_into()?;

            Ok(profiles.RegisterProfile(
                &GUID_TEXT_SERVICE,
                langid,
                &GUID_PROFILE,
                SERVICE_NAME.to_wide_16().as_slice(),
                dll_path.to_wide_16().as_slice(),
                0,
                HKL::default(),
                0,
                true,
                0,
            )?)
        }
    }

    pub fn unregister() -> anyhow::Result<()> {
        unsafe {
            let profiles: ITfInputProcessorProfileMgr =
                CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;

            let langid: u16 = LocaleNameToLCID(w!("ja-JP"), 0).try_into()?;

            Ok(profiles.UnregisterProfile(&GUID_TEXT_SERVICE, langid, &GUID_PROFILE, 0)?)
        }
    }
}

// register the CLSID(unique id for textservice) of the TextService
pub struct CLSIDMgr;
impl CLSIDMgr {
    pub fn register(dll_path: &str) -> anyhow::Result<()> {
        let clsid_key = CLSID_PREFIX.to_owned() + &GUID_TEXT_SERVICE.to_string();
        let inproc_key = clsid_key.clone() + INPROC_SUFFIX;

        let hkey = HKEY_CLASSES_ROOT.create_subkey(&clsid_key)?;
        hkey.set_string("", SERVICE_NAME)?;
        hkey.close()?;

        let inproc_hkey = HKEY_CLASSES_ROOT.create_subkey(&inproc_key)?;
        inproc_hkey.set_string("", dll_path)?;
        inproc_hkey.set_string("ThreadingModel", "Apartment")?;
        inproc_hkey.close()?;

        Ok(())
    }

    pub fn unregister() -> anyhow::Result<()> {
        let clsid_key = CLSID_PREFIX.to_owned() + &GUID_TEXT_SERVICE.to_string();

        // RegDeleteTreeW removes the whole subtree, InProcServer32 included —
        // a second delete of the subkey would fail with "not found" and made
        // regsvr32 /u always report 0x80040201. Tolerating "not found" also
        // makes /u idempotent (running it twice must succeed).
        ok_if_not_found(HKEY_CLASSES_ROOT.delete_tree(&clsid_key))?;

        Ok(())
    }
}

/// Unregistration must be idempotent: deleting something that is already gone
/// is success, while every other error (e.g. access denied without admin
/// rights) must still surface.
fn ok_if_not_found(result: windows::core::Result<()>) -> windows::core::Result<()> {
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    match result {
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(()),
        other => other,
    }
}

// register the category (entitlements of textservice) of the TextService
pub struct CategoryMgr;
impl CategoryMgr {
    const CATEGORIES: [GUID; 7] = [
        GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
        GUID_TFCAT_TIPCAP_COMLESS,
        GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
        GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
        GUID_TFCAT_TIP_KEYBOARD,
        GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
        GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
    ];

    pub fn register() -> anyhow::Result<()> {
        unsafe {
            let catmgr: ITfCategoryMgr =
                CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?;

            for cat in Self::CATEGORIES.iter() {
                catmgr.RegisterCategory(&GUID_TEXT_SERVICE, cat, &GUID_TEXT_SERVICE)?;
            }

            Ok(())
        }
    }

    pub fn unregister() -> anyhow::Result<()> {
        unsafe {
            let catmgr: ITfCategoryMgr =
                CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?;

            for cat in Self::CATEGORIES.iter() {
                catmgr.UnregisterCategory(&GUID_TEXT_SERVICE, cat, &GUID_TEXT_SERVICE)?;
            }

            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{E_ACCESSDENIED, ERROR_FILE_NOT_FOUND};

    /// An already-deleted key is success — regsvr32 /u must be idempotent.
    #[test]
    fn not_found_is_tolerated() {
        let result = ok_if_not_found(Err(windows::core::Error::from_hresult(
            ERROR_FILE_NOT_FOUND.to_hresult(),
        )));
        assert!(result.is_ok(), "not-found must be treated as success");
    }

    /// A real failure (e.g. regsvr32 /u without admin rights) must surface.
    #[test]
    fn other_errors_still_surface() {
        let result = ok_if_not_found(Err(windows::core::Error::from_hresult(E_ACCESSDENIED)));
        assert_eq!(result.unwrap_err().code(), E_ACCESSDENIED);
    }

    #[test]
    fn success_passes_through() {
        assert!(ok_if_not_found(Ok(())).is_ok());
    }
}
