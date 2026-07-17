use std::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};
use windows::{
    core::{implement, BSTR, GUID},
    Win32::{
        Foundation::{E_FAIL, E_POINTER, S_FALSE},
        UI::TextServices::{
            IEnumTfDisplayAttributeInfo, IEnumTfDisplayAttributeInfo_Impl, ITfDisplayAttributeInfo,
            ITfDisplayAttributeInfo_Impl, ITfDisplayAttributeProvider_Impl, TF_DISPLAYATTRIBUTE,
        },
    },
};

use anyhow::Result;

use crate::globals::{DISPLAY_ATTRIBUTE, GUID_DISPLAY_ATTRIBUTE};

use super::factory::TextServiceFactory_Impl;

// class for display attribute (color, bold, underline, etc.)
impl ITfDisplayAttributeProvider_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn EnumDisplayAttributeInfo(&self) -> windows::core::Result<IEnumTfDisplayAttributeInfo> {
        let enum_info = EnumDisplayAttributeInfo::new();
        Ok(enum_info.into())
    }

    #[macros::anyhow]
    fn GetDisplayAttributeInfo(
        &self,
        guid: *const windows_core::GUID,
    ) -> windows::core::Result<ITfDisplayAttributeInfo> {
        let guid = unsafe { *guid };
        let attributes = EnumDisplayAttributeInfo::new();
        for attribute in attributes.attributes {
            if attribute.guid == guid {
                return Ok(attribute.into());
            }
        }
        anyhow::bail!("Display attribute not found");
    }
}

#[derive(Clone)]
#[implement(ITfDisplayAttributeInfo)]
pub struct DisplayAttributeInfo {
    pub guid: GUID,
    attribute: Cell<TF_DISPLAYATTRIBUTE>,
    attribute_backup: TF_DISPLAYATTRIBUTE,
}

impl DisplayAttributeInfo {
    pub fn new(guid: GUID, attribute: TF_DISPLAYATTRIBUTE) -> Self {
        DisplayAttributeInfo {
            guid,
            attribute: Cell::new(attribute),
            attribute_backup: attribute,
        }
    }
}

impl ITfDisplayAttributeInfo_Impl for DisplayAttributeInfo_Impl {
    #[macros::anyhow]
    fn GetAttributeInfo(&self, pda: *mut TF_DISPLAYATTRIBUTE) -> Result<()> {
        unsafe {
            *pda = self.attribute.get();
        }
        Ok(())
    }

    #[macros::anyhow]
    fn GetGUID(&self) -> Result<GUID> {
        Ok(self.guid)
    }

    #[macros::anyhow]
    fn Reset(&self) -> Result<()> {
        self.attribute.set(self.attribute_backup);
        Ok(())
    }

    #[macros::anyhow]
    fn GetDescription(&self) -> Result<BSTR> {
        Ok(BSTR::default())
    }

    #[macros::anyhow]
    fn SetAttributeInfo(&self, pda: *const TF_DISPLAYATTRIBUTE) -> Result<()> {
        unsafe {
            self.attribute.set(*pda);
        }
        Ok(())
    }
}

#[implement(IEnumTfDisplayAttributeInfo)]
pub struct EnumDisplayAttributeInfo {
    pub attributes: Vec<DisplayAttributeInfo>,
    index: AtomicUsize,
}

#[allow(clippy::new_without_default)]
impl EnumDisplayAttributeInfo {
    pub fn new() -> Self {
        let attributes = vec![DisplayAttributeInfo::new(
            GUID_DISPLAY_ATTRIBUTE,
            DISPLAY_ATTRIBUTE,
        )];

        EnumDisplayAttributeInfo {
            attributes,
            index: AtomicUsize::new(0),
        }
    }
}

impl IEnumTfDisplayAttributeInfo_Impl for EnumDisplayAttributeInfo_Impl {
    #[macros::anyhow]
    fn Clone(&self) -> Result<IEnumTfDisplayAttributeInfo> {
        let clone = EnumDisplayAttributeInfo::new();
        clone.index.store(self.index.load(Relaxed), Relaxed);
        Ok(clone.into())
    }

    // Not #[macros::anyhow]: the IEnumXxx contract requires S_FALSE when
    // fewer than `ulcount` items are returned, but the anyhow wrapper
    // collapses every Ok into S_OK — a host looping `while Next(1) == S_OK`
    // would then never terminate. Panics are still caught and turned into
    // E_FAIL, exactly as the wrapper would.
    fn Next(
        &self,
        ulcount: u32,
        rginfo: *mut Option<ITfDisplayAttributeInfo>,
        pcfetched: *mut u32,
    ) -> windows::core::Result<()> {
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            if ulcount == 0 {
                if !pcfetched.is_null() {
                    *pcfetched = 0;
                }
                return Ok(());
            }
            if rginfo.is_null() {
                return Err(windows::core::Error::from_hresult(E_POINTER));
            }

            let mut fetched: u32 = 0;
            let mut index = self.index.load(Relaxed);

            while fetched < ulcount && index < self.attributes.len() {
                let attribute = self.attributes[index].clone();
                // write into the fetched-th slot: `.add` ADVANCES the output
                // pointer per item (a plain `*rginfo =` kept overwriting slot
                // 0), and `.write` moves the value in WITHOUT dropping the
                // uninitialized garbage the caller's buffer may hold (a plain
                // assignment would run Drop/Release on it)
                rginfo.add(fetched as usize).write(Some(attribute.into()));
                fetched += 1;
                index += 1;
            }

            self.index.store(index, Relaxed);

            // pcfetched may legally be null when ulcount == 1
            if !pcfetched.is_null() {
                *pcfetched = fetched;
            }

            if fetched < ulcount {
                Err(windows::core::Error::from_hresult(S_FALSE))
            } else {
                Ok(())
            }
        }));

        match run {
            Ok(result) => result,
            Err(_) => Err(windows::core::Error::from_hresult(E_FAIL)),
        }
    }

    #[macros::anyhow]
    fn Reset(&self) -> Result<()> {
        self.index.store(0, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    #[macros::anyhow]
    fn Skip(&self, ulcount: u32) -> Result<()> {
        // clamp: Next relies on `index < len`, and an unchecked add could
        // also overflow on a hostile count
        let len = self.attributes.len();
        let index = self
            .index
            .load(Relaxed)
            .saturating_add(ulcount as usize)
            .min(len);
        self.index.store(index, Relaxed);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn enumerator(n: usize) -> IEnumTfDisplayAttributeInfo {
        let attributes = (0..n)
            .map(|_| DisplayAttributeInfo::new(GUID_DISPLAY_ATTRIBUTE, DISPLAY_ATTRIBUTE))
            .collect();
        EnumDisplayAttributeInfo {
            attributes,
            index: AtomicUsize::new(0),
        }
        .into()
    }

    /// The core bug: `Next` must advance its output pointer and fill each
    /// requested slot. The old `*rginfo =` kept writing slot 0, so with more
    /// than one attribute every slot but the first came back empty.
    #[test]
    fn next_fills_every_slot() {
        let e = enumerator(3);
        let mut slots: [Option<ITfDisplayAttributeInfo>; 3] = [None, None, None];
        let mut fetched = 0u32;

        unsafe { e.Next(&mut slots, &mut fetched) }.expect("Next failed");

        assert_eq!(fetched, 3);
        assert!(
            slots.iter().all(Option::is_some),
            "every requested slot must be filled, got {:?}",
            slots.iter().map(Option::is_some).collect::<Vec<_>>()
        );
    }

    /// Asking for more than exist fills what it can and reports the count.
    #[test]
    fn next_partial_fill_reports_fetched() {
        let e = enumerator(1);
        let mut slots: [Option<ITfDisplayAttributeInfo>; 3] = [None, None, None];
        let mut fetched = 0u32;

        // S_FALSE is a success HRESULT, so the windows-rs wrapper returns Ok;
        // the caller distinguishes partial results via `fetched`
        let _ = unsafe { e.Next(&mut slots, &mut fetched) };

        assert_eq!(fetched, 1);
        assert!(slots[0].is_some());
        assert!(slots[1].is_none() && slots[2].is_none());
    }

    /// Once exhausted, `Next` yields nothing rather than re-emitting.
    #[test]
    fn next_is_exhausted_after_all_items() {
        let e = enumerator(1);
        let mut first = [None];
        let mut fetched = 0u32;
        unsafe { e.Next(&mut first, &mut fetched) }.expect("first Next failed");
        assert_eq!(fetched, 1);

        let mut second = [None];
        let mut fetched2 = 0u32;
        let _ = unsafe { e.Next(&mut second, &mut fetched2) };
        assert_eq!(fetched2, 0);
        assert!(second[0].is_none());
    }

    /// Skipping past the end must not leave the index out of range.
    #[test]
    fn skip_past_the_end_is_clamped() {
        let e = enumerator(1);
        unsafe { e.Skip(100) }.expect("Skip failed");

        let mut slots = [None];
        let mut fetched = 0u32;
        let _ = unsafe { e.Next(&mut slots, &mut fetched) };
        assert_eq!(fetched, 0);
    }
}
