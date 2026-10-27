//! API keys under user-scoped DPAPI (design 8.7, contract section 7).
//!
//! Keys arrive only through the core's dedicated method, never as Skill input,
//! prompt text or worker data. Each key is sealed with CryptProtectData for the
//! current Windows user plus secret per-vault entropy (format 2) derived from
//! the vault master key, stored outside the vault database (so encrypted
//! backups never contain it) and decrypted only for the duration of one
//! request. Without the unlocked vault, another process of the same user
//! cannot unseal a key; while the vault is unlocked DPAPI is still not a
//! same-user isolation boundary for the core's own memory.
use crate::{Error, Result, crypto::SecretKey};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub struct CredentialStore {
    directory: PathBuf,
    entropy: Zeroizing<Vec<u8>>,
    /// Format 1 entropy (public vault id), accepted once and re-sealed.
    legacy: Vec<u8>,
}

impl CredentialStore {
    /// `data_root/credentials/<vault_id>/`; the entropy binds files to the
    /// vault and is a secret only the unlocked vault can derive.
    pub fn for_vault(data_root: &Path, vault_id: &str, secret: &SecretKey) -> Result<Self> {
        if vault_id.is_empty() || !vault_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(Error::Integrity);
        }
        let mut entropy = Zeroizing::new(format!("lvjin.credential/2:{vault_id}:").into_bytes());
        entropy.extend_from_slice(&secret.0);
        Ok(Self { directory: data_root.join("credentials").join(vault_id), entropy, legacy: format!("lvjin.credential/1:{vault_id}").into_bytes() })
    }

    fn path(&self, credential: &str) -> Result<PathBuf> {
        if credential.len() != 37 || !credential.starts_with("cred_") || !credential[5..].bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Denied);
        }
        Ok(self.directory.join(format!("{credential}.dpapi")))
    }

    pub fn new_id() -> String {
        format!("cred_{}", uuid::Uuid::new_v4().simple())
    }

    pub fn put(&self, credential: &str, secret: &Zeroizing<String>) -> Result<()> {
        self.seal(credential, secret.as_bytes())
    }

    fn seal(&self, credential: &str, plain: &[u8]) -> Result<()> {
        let sealed = protect(plain, &self.entropy)?;
        std::fs::create_dir_all(&self.directory)?;
        crate::durable::write(&self.path(credential)?, &sealed)
    }

    pub fn get(&self, credential: &str) -> Result<Zeroizing<String>> {
        let sealed = std::fs::read(self.path(credential)?).map_err(|_| Error::NeedsInput("api_key"))?;
        let plain = match unprotect(&sealed, &self.entropy) {
            Ok(plain) => plain,
            Err(_) => {
                // Sealed before the secret entropy existed: open it once with the
                // public format-1 entropy and re-seal it under the secret one.
                let plain = unprotect(&sealed, &self.legacy)?;
                self.seal(credential, &plain)?;
                plain
            }
        };
        let text = std::str::from_utf8(&plain).map_err(|_| Error::Crypto)?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    /// Re-seal every format-1 key of this vault (run once after unlocking).
    /// Returns how many keys were migrated; unreadable files are left alone.
    pub fn migrate_legacy(&self) -> Result<usize> {
        let Ok(entries) = std::fs::read_dir(&self.directory) else { return Ok(0) };
        let mut migrated = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(credential) = name.strip_suffix(".dpapi") else { continue };
            let Ok(sealed) = std::fs::read(entry.path()) else { continue };
            if unprotect(&sealed, &self.entropy).is_ok() {
                continue;
            }
            if let Ok(plain) = unprotect(&sealed, &self.legacy) {
                if self.path(credential).is_ok() && self.seal(credential, &plain).is_ok() {
                    migrated += 1;
                }
            }
        }
        Ok(migrated)
    }

    /// Seal with the format-1 public entropy (only for the migration tests).
    #[cfg(feature = "test-harness")]
    pub fn put_legacy_for_test(&self, credential: &str, secret: &Zeroizing<String>) -> Result<()> {
        let sealed = protect(secret.as_bytes(), &self.legacy)?;
        std::fs::create_dir_all(&self.directory)?;
        crate::durable::write(&self.path(credential)?, &sealed)
    }

    pub fn exists(&self, credential: &str) -> bool {
        self.path(credential).map(|p| p.is_file()).unwrap_or(false)
    }

    /// Overwrite then delete. Physical remnants on SSDs are not guaranteed gone.
    pub fn remove(&self, credential: &str) -> Result<()> {
        let path = self.path(credential)?;
        if let Ok(metadata) = std::fs::metadata(&path) {
            let _ = std::fs::write(&path, vec![0u8; metadata.len() as usize]);
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn protect(data: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    };
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let extra = CRYPT_INTEGER_BLOB { cbData: entropy.len() as u32, pbData: entropy.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe { CryptProtectData(&input, std::ptr::null(), &extra, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) };
    if ok == 0 || output.pbData.is_null() {
        return Err(Error::Crypto);
    }
    let sealed = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(output.pbData.cast()) };
    Ok(sealed)
}

#[cfg(windows)]
fn unprotect(sealed: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData},
    };
    let input = CRYPT_INTEGER_BLOB { cbData: sealed.len() as u32, pbData: sealed.as_ptr() as *mut u8 };
    let extra = CRYPT_INTEGER_BLOB { cbData: entropy.len() as u32, pbData: entropy.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe { CryptUnprotectData(&input, std::ptr::null_mut(), &extra, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) };
    if ok == 0 || output.pbData.is_null() {
        return Err(Error::Crypto);
    }
    let length = output.cbData as usize;
    let plain = Zeroizing::new(unsafe { std::slice::from_raw_parts(output.pbData, length) }.to_vec());
    unsafe {
        std::ptr::write_bytes(output.pbData, 0, length);
        LocalFree(output.pbData.cast());
    }
    Ok(plain)
}

#[cfg(not(windows))]
fn protect(_: &[u8], _: &[u8]) -> Result<Vec<u8>> {
    Err(Error::Crypto)
}

#[cfg(not(windows))]
fn unprotect(_: &[u8], _: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    Err(Error::Crypto)
}
