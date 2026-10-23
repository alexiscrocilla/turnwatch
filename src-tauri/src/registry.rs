//! String values under `HKEY_CURRENT_USER`, the only part of the registry the
//! app touches (its toast identity, its start with Windows). Win32 error
//! codes are returned as is; other systems have no registry and always fail.

#[cfg(windows)]
mod imp {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW,
        RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// Creates `HKEY_CURRENT_USER\<subkey>` if needed and sets each string
    /// value that differs. Returns the first Win32 error.
    pub fn write_values(subkey: &str, values: &[(&str, String)]) -> Result<(), u32> {
        let subkey = wide(subkey);
        let mut key: HKEY = null_mut();
        // SAFETY: every pointer is a live null-terminated buffer or an out
        // parameter for the duration of the call; the key is closed below.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                null(),
                &mut key,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        let mut result = Ok(());
        for (name, value) in values {
            if read_string(key, name).as_deref() == Some(value.as_str()) {
                continue;
            }
            let name = wide(name);
            let data = wide(value);
            let Ok(bytes) = u32::try_from(data.len() * 2) else {
                continue;
            };
            // SAFETY: `data` holds `bytes` bytes, null terminator included.
            let status = unsafe {
                RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), bytes)
            };
            if status != ERROR_SUCCESS && result.is_ok() {
                result = Err(status);
            }
        }
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        result
    }

    /// The string value `name` of `HKEY_CURRENT_USER\<subkey>`, if present.
    pub fn read_value(subkey: &str, name: &str) -> Option<String> {
        let subkey = wide(subkey);
        let mut key: HKEY = null_mut();
        // SAFETY: `subkey` is null-terminated; `key` is an out parameter,
        // closed below.
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_QUERY_VALUE, &mut key) };
        if status != ERROR_SUCCESS {
            return None;
        }
        let value = read_string(key, name);
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        value
    }

    /// The binary value `name` of `HKEY_CURRENT_USER\<subkey>`, if present.
    pub fn read_binary(subkey: &str, name: &str) -> Option<Vec<u8>> {
        let subkey = wide(subkey);
        let name = wide(name);
        let mut size = 0u32;
        // SAFETY: size query; both buffers are null-terminated, no data buffer.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_BINARY,
                null_mut(),
                null_mut(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: `buffer` holds `size` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_BINARY,
                null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        buffer.truncate(size as usize);
        Some(buffer)
    }

    /// Removes the value `name` of `HKEY_CURRENT_USER\<subkey>`; a value or
    /// key already missing is not an error.
    pub fn delete_value(subkey: &str, name: &str) -> Result<(), u32> {
        let subkey = wide(subkey);
        let name = wide(name);
        // SAFETY: both buffers are null-terminated for the duration of the call.
        let status =
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), name.as_ptr()) };
        match status {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            other => Err(other),
        }
    }

    /// A string value of an open key, if present.
    fn read_string(key: HKEY, name: &str) -> Option<String> {
        let name = wide(name);
        let mut size = 0u32;
        // SAFETY: size query; no data buffer is passed.
        let status = unsafe {
            RegGetValueW(key, null(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size)
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2).max(1)];
        let mut size = u32::try_from(buffer.len() * 2).ok()?;
        // SAFETY: `buffer` holds `size` bytes.
        let status = unsafe {
            RegGetValueW(
                key,
                null(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        buffer.truncate((size as usize / 2).min(buffer.len()));
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        String::from_utf16(&buffer).ok()
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn write_values(_subkey: &str, _values: &[(&str, String)]) -> Result<(), u32> {
        Err(0)
    }

    pub fn read_value(_subkey: &str, _name: &str) -> Option<String> {
        None
    }

    pub fn read_binary(_subkey: &str, _name: &str) -> Option<Vec<u8>> {
        None
    }

    pub fn delete_value(_subkey: &str, _name: &str) -> Result<(), u32> {
        Err(0)
    }
}

pub use imp::{delete_value, read_binary, read_value, write_values};
