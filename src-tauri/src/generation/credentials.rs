//! API keys under user-scoped DPAPI (design 8.7, contract section 7).
//!
//! Keys arrive only through the core's dedicated method, never as Skill input,
//! prompt text or worker data. Each key is sealed with CryptProtectData for the
//! current Windows user plus per-vault entropy, stored outside the vault
//! database (so encrypted backups never contain it) and decrypted only for the
//! duration of one request. DPAPI protects the file at rest; it is not a
//! same-user process isolation boundary.
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub struct CredentialStore {
    directory: PathBuf,
    entropy: Vec<u8>,
}

impl CredentialStore {
    /// `data_root/credentials/<vault_id>/`; entropy binds files to the vault.
    pub fn for_vault(data_root: &Path, vault_id: &str) -> Result<Self> {
        if vault_id.is_empty() || !vault_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(Error::Integrity);
        }
        Ok(Self { directory: data_root.join("credentials").join(vault_id), entropy: format!("lvjin.credential/1:{vault_id}").into_bytes() })
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
        let sealed = protect(secret.as_bytes(), &self.entropy)?;
        std::fs::create_dir_all(&self.directory)?;
        let target = self.path(credential)?;
        let temporary = target.with_extension("dpapi.tmp");
        std::fs::write(&temporary, &sealed)?;
        std::fs::rename(&temporary, &target)?;
        Ok(())
    }

    pub fn get(&self, credential: &str) -> Result<Zeroizing<String>> {
        let sealed = std::fs::read(self.path(credential)?).map_err(|_| Error::NeedsInput("api_key"))?;
        let plain = unprotect(&sealed, &self.entropy)?;
        let text = std::str::from_utf8(&plain).map_err(|_| Error::Crypto)?;
        Ok(Zeroizing::new(text.to_owned()))
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
