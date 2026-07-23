use windows::{
    Win32::{
        System::Registry::{
            HKEY, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
            RegDeleteTreeW, RegSetValueExW,
        },
        UI::Input::KeyboardAndMouse::{GetKeyState, VIRTUAL_KEY},
    },
    core::{GUID, HSTRING, PCWSTR},
};

use crate::check_win32_err;

// string extension
pub trait StringExt {
    fn to_wide_16(&self) -> Vec<u16>;
    fn to_wide_16_unpadded(&self) -> Vec<u16>;
    fn to_wide(&self) -> Vec<u8>;
}

impl StringExt for &str {
    fn to_wide_16(&self) -> Vec<u16> {
        self.encode_utf16().chain(Some(0)).collect()
    }

    fn to_wide_16_unpadded(&self) -> Vec<u16> {
        self.encode_utf16().collect()
    }

    fn to_wide(&self) -> Vec<u8> {
        // REG_SZ requires a two-byte (UTF-16) NUL terminator. Build the wide
        // string with a U+0000 terminator first, then widen to bytes, so the
        // result stays even-length; appending a single 0 byte would leave an
        // odd-length buffer with a one-byte terminator.
        self.to_wide_16()
            .into_iter()
            .flat_map(|c| c.to_le_bytes())
            .collect()
    }
}

// guid extension
pub trait GUIDExt {
    fn to_string(&self) -> String;
}

impl GUIDExt for GUID {
    fn to_string(&self) -> String {
        format!(
            "{{{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
            self.data1,
            self.data2,
            self.data3,
            self.data4[0],
            self.data4[1],
            self.data4[2],
            self.data4[3],
            self.data4[4],
            self.data4[5],
            self.data4[6],
            self.data4[7],
        )
    }
}

// registry extension

pub trait RegKey {
    fn create_subkey(&self, subkey: &str) -> windows::core::Result<HKEY>;
    fn set_string(&self, value_name: &str, value: &str) -> windows::core::Result<()>;
    fn delete_tree(&self, subkey: &str) -> windows::core::Result<()>;
    fn close(&self) -> windows::core::Result<()>;
}

impl RegKey for HKEY {
    fn create_subkey(&self, subkey_name: &str) -> windows::core::Result<HKEY> {
        let subkey_name_w = HSTRING::from(subkey_name);
        let mut subkey_handle: HKEY = HKEY::default();

        unsafe {
            let result = RegCreateKeyExW(
                *self,
                PCWSTR(subkey_name_w.as_ptr()),
                // the reserved parameter, modelled as an Option since 0.62
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut subkey_handle,
                None,
            );

            check_win32_err!(result, subkey_handle)
        }
    }

    fn set_string(&self, value_name: &str, value: &str) -> windows::core::Result<()> {
        let value_name_w = HSTRING::from(value_name);
        let value_w = value.to_wide();
        unsafe {
            let result = RegSetValueExW(
                *self,
                PCWSTR(value_name_w.as_ptr()),
                // ditto: reserved
                None,
                REG_SZ,
                Some(value_w.as_slice()),
            );

            check_win32_err!(result)
        }
    }

    fn delete_tree(&self, subkey: &str) -> windows::core::Result<()> {
        let subkey_w = HSTRING::from(subkey);
        unsafe {
            let result = RegDeleteTreeW(*self, PCWSTR(subkey_w.as_ptr()));

            check_win32_err!(result)
        }
    }

    fn close(&self) -> windows::core::Result<()> {
        unsafe {
            let result = RegCloseKey(*self);
            check_win32_err!(result)
        }
    }
}

#[allow(clippy::wrong_self_convention)]
pub trait VKeyExt {
    fn is_pressed(self) -> bool;
}

impl VKeyExt for VIRTUAL_KEY {
    fn is_pressed(self) -> bool {
        unsafe { GetKeyState(self.0 as i32) as u16 & 0x8000 != 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::{GUIDExt, StringExt};
    use windows::core::GUID;

    #[test]
    fn to_wide_is_even_length_with_two_byte_nul_terminator() {
        let bytes = "ab".to_wide();
        // 2 chars * 2 bytes + 2-byte NUL = 6 bytes, even length.
        assert_eq!(bytes, vec![0x61, 0x00, 0x62, 0x00, 0x00, 0x00]);
        assert_eq!(bytes.len() % 2, 0);
        assert_eq!(&bytes[bytes.len() - 2..], &[0x00, 0x00]);
    }

    #[test]
    fn to_wide_terminates_empty_string_with_two_byte_nul() {
        assert_eq!("".to_wide(), vec![0x00, 0x00]);
    }

    #[test]
    fn to_wide_keeps_even_length_for_non_ascii() {
        let bytes = "あ🌟".to_wide();
        // Non-BMP characters widen to surrogate pairs; the buffer must stay
        // even-length and end in a two-byte NUL regardless.
        assert_eq!(bytes.len() % 2, 0);
        assert_eq!(&bytes[bytes.len() - 2..], &[0x00, 0x00]);
    }

    /// The registry format, hand-rolled from the `data4` bytes. `register.rs`
    /// builds `CLSID\{...}` keys out of this, so a formatting slip does not
    /// misbehave subtly — it registers the IME under a key TSF never looks up.
    #[test]
    fn guid_is_formatted_as_a_braced_registry_string() {
        let guid = GUID::from_u128(0xffdefe79_2fc2_11ef_b16b_94e70b2c378c);

        assert_eq!(
            GUIDExt::to_string(&guid),
            "{ffdefe79-2fc2-11ef-b16b-94e70b2c378c}"
        );
    }

    /// Every byte is zero-padded: a `data4` byte below 0x10 must not shorten
    /// the string (`{...-0a...}`, never `{...-a...}`), and neither may the
    /// leading groups.
    #[test]
    fn guid_groups_are_zero_padded() {
        let guid = GUID::from_u128(0x00000001_0002_0003_0405_060708090a0b);

        assert_eq!(
            GUIDExt::to_string(&guid),
            "{00000001-0002-0003-0405-060708090a0b}"
        );
    }

    /// 8-4-4-4-12 hex digits between braces, for any value.
    #[test]
    fn guid_string_has_the_canonical_shape() {
        let formatted = GUIDExt::to_string(&GUID::from_u128(u128::MAX));

        assert_eq!(formatted, "{ffffffff-ffff-ffff-ffff-ffffffffffff}");
        assert_eq!(formatted.len(), 38);
    }
}
