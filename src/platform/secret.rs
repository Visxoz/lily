//! Keeps sign-in data encrypted at rest with Windows' DPAPI, so other
//! Windows users and copies of the files on other machines cannot read it.

use std::{
    io,
    path::{Path, PathBuf},
    ptr,
};

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

/// Mixed into the key, so other programs calling DPAPI for this user
/// without it cannot decrypt the files. Kept from before the app was named
/// Lily, as changing it would sign everyone out.
const ENTROPY: &[u8] = b"ytm-native sign-in";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    }
}

fn crypt(data: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    // SAFETY: the input blobs point at live slices, which DPAPI only reads,
    // and `output` receives a buffer DPAPI allocates.
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 || output.pbData.is_null() {
        return None;
    }
    // SAFETY: DPAPI returned `cbData` bytes at `pbData`, which are copied
    // out and then freed once with `LocalFree`, as it requires.
    unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        Some(bytes)
    }
}

pub fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(crypt(&bytes, false)?).ok()
}

/// The old file stays whole should this fail.
pub fn write(path: &Path, text: &str) -> io::Result<()> {
    let data = crypt(text.as_bytes(), true)
        .ok_or_else(|| io::Error::other("Windows could not encrypt the data"))?;
    crate::settings::write_atomic(path, &data)
}

/// rustypipe's cache, which holds the sign-in cookie.
pub struct EncryptedStorage {
    path: PathBuf,
}

impl EncryptedStorage {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("rustypipe_cache.bin"),
        }
    }
}

impl rustypipe::cache::CacheStorage for EncryptedStorage {
    fn write(&self, data: &str) {
        if let Err(e) = write(&self.path, data) {
            log::warn!("could not save rustypipe's cache: {e}");
            // Signing out: a stale copy would bring the removed cookie back.
            if !data.contains("\"auth_cookie\"") {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    fn read(&self) -> Option<String> {
        read(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_encrypted() {
        let dir = std::env::temp_dir().join(format!("ytm-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.bin");

        write(&path, "SID=a; HSID=b").unwrap();
        let stored = std::fs::read(&path).unwrap();
        assert!(
            !stored.windows(5).any(|w| w == b"SID=a"),
            "stored encrypted"
        );
        assert_eq!(read(&path).as_deref(), Some("SID=a; HSID=b"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
