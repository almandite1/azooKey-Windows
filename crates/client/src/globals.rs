use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
};

use anyhow::{Context, Result};

use windows::{
    Win32::{
        Foundation::{FALSE, HMODULE, MAX_PATH},
        System::LibraryLoader::GetModuleFileNameW,
        UI::TextServices::{
            TF_ATTR_TARGET_CONVERTED, TF_CT_NONE, TF_DA_COLOR, TF_DA_COLOR_0, TF_DISPLAYATTRIBUTE,
            TF_LS_SOLID,
        },
    },
    core::GUID,
};

pub const CLSID_PREFIX: &str = "CLSID\\";
pub const INPROC_SUFFIX: &str = "\\InProcServer32";

pub const SERVICE_NAME: &str = "Azookey";

// ffdefe79-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_TEXT_SERVICE: GUID = GUID::from_u128(0xffdefe79_2fc2_11ef_b16b_94e70b2c378c);
// ffdefe7a-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_PROFILE: GUID = GUID::from_u128(0xffdefe7a_2fc2_11ef_b16b_94e70b2c378c);

// DisplayAttribute用のGUID
pub const GUID_DISPLAY_ATTRIBUTE: GUID = GUID::from_u128(0xffdefe7b_2fc2_11ef_b16b_94e70b2c378c);

/// Identifies our candidate list to `ITfUIElementMgr`. Must be TIP-specific
/// and stable — deliberately NOT `GUID_TEXT_SERVICE`, which identifies the
/// text service itself rather than this UI element.
// ffdefe7c-2fc2-11ef-b16b-94e70b2c378c
pub const GUID_CANDIDATE_LIST_UI_ELEMENT: GUID =
    GUID::from_u128(0xffdefe7c_2fc2_11ef_b16b_94e70b2c378c);

pub const DISPLAY_ATTRIBUTE: TF_DISPLAYATTRIBUTE = TF_DISPLAYATTRIBUTE {
    crText: TF_DA_COLOR {
        r#type: TF_CT_NONE,
        Anonymous: TF_DA_COLOR_0 { nIndex: 0 },
    },
    crBk: TF_DA_COLOR {
        r#type: TF_CT_NONE,
        Anonymous: TF_DA_COLOR_0 { nIndex: 0 },
    },
    lsStyle: TF_LS_SOLID,
    fBoldLine: FALSE,
    crLine: TF_DA_COLOR {
        r#type: TF_CT_NONE,
        Anonymous: TF_DA_COLOR_0 { nIndex: 0 },
    },
    bAttr: TF_ATTR_TARGET_CONVERTED,
};

// You can use any value for this cookie.
pub const TEXTSERVICE_LANGBARITEMSINK_COOKIE: u32 = 0;

pub static DLL_INSTANCE: OnceLock<Mutex<DllModule>> = OnceLock::new();

unsafe impl Sync for DllModule {}
unsafe impl Send for DllModule {}

#[derive(Debug)]
pub struct DllModule {
    pub ref_count: Arc<AtomicUsize>,
    pub hinst: Option<HMODULE>,
    pub sender: Option<Sender<bool>>,
}

impl DllModule {
    pub fn new() -> Self {
        Self {
            ref_count: Arc::new(AtomicUsize::new(0)),
            hinst: None,
            sender: None,
        }
    }

    pub fn get() -> Result<MutexGuard<'static, DllModule>> {
        // recover from poisoning: one panic while the lock was held must
        // not permanently break every later Activate/LockServer call
        Ok(DLL_INSTANCE
            .get()
            .ok_or_else(|| anyhow::anyhow!("DllModule is not initialized"))?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub fn get_path() -> anyhow::Result<String> {
        let dll_instance = DllModule::get()?.hinst.context("Dll instance not found")?;

        // GetModuleFileNameW does not report the required length: it fills the
        // buffer, and if the path does not fit it truncates and returns the
        // buffer size. A single fixed MAX_PATH call therefore silently yields a
        // truncated path for deep install locations. Grow the buffer until the
        // returned length is strictly less than its size (i.e. it fit).
        let mut buffer: Vec<u16> = vec![0; MAX_PATH as usize];
        loop {
            let length = unsafe { GetModuleFileNameW(Some(dll_instance), &mut buffer) } as usize;

            if length == 0 {
                return Err(anyhow::anyhow!("GetModuleFileNameW failed: {:?}", unsafe {
                    windows::Win32::Foundation::GetLastError()
                }));
            }

            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(String::from_utf16_lossy(&buffer));
            }

            // length == buffer.len(): the path was truncated. Cap growth at the
            // Windows extended-length maximum (32767 + NUL) so a wrong result
            // cannot loop forever.
            if buffer.len() >= 0x8000 {
                return Err(anyhow::anyhow!(
                    "module path exceeds {} UTF-16 code units",
                    buffer.len()
                ));
            }
            buffer.resize(buffer.len() * 2, 0);
        }
    }

    pub fn add_ref(&mut self) -> usize {
        self.ref_count.fetch_add(1, Ordering::SeqCst)
    }

    pub fn release(&mut self) -> usize {
        self.ref_count.fetch_sub(1, Ordering::SeqCst)
    }
}
