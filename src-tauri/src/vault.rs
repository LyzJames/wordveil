//! Private vault header, envelope key wrapping and purpose keys (design 12.2).
//!
//! A random 256-bit master key is wrapped twice: by an Argon2id key derived
//! from the passphrase and by a high-entropy recovery code shown once at
//! creation. Database, object, index and backup keys are HKDF purpose keys. The
//! header holds no secret; its parameters are bound into the wrap AAD so they
//! cannot be weakened without breaking authentication.
use crate::{
    Error, Result,
    crypto::{KdfParams, SecretKey, passphrase_kek, random_bytes},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub const HEADER_FILE: &str = "vault.json";
pub const DB_FILE: &str = "vault.db";
pub const OBJECTS_DIR: &str = "objects";
const SCHEMA: &str = "lvjin.vault/1";
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KdfRecord {
    pub algorithm: String,
    pub params: KdfParams,
    pub salt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Formats {
    pub database: u32,
    pub objects: u32,
    pub index: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultHeader {
    pub schema_version: String,
    pub vault_id: String,
    pub created_at: String,
    pub kdf: KdfRecord,
    pub passphrase_wrap: String,
    pub recovery_salt: String,
    pub recovery_wrap: String,
    pub formats: Formats,
}

impl VaultHeader {
    fn passphrase_aad(&self) -> Vec<u8> {
        let p = self.kdf.params;
        format!("lvjin-vault/1|{}|passphrase|argon2id-v19|{}|{}|{}", self.vault_id, p.memory_kib, p.iterations, p.parallelism).into_bytes()
    }
    fn recovery_aad(&self) -> Vec<u8> {
        format!("lvjin-vault/1|{}|recovery", self.vault_id).into_bytes()
    }
    fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA
            || self.vault_id.len() != 32
            || !self.vault_id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || self.kdf.algorithm != "argon2id-v19"
            || !self.kdf.params.meets_floor()
            || self.formats.database != 1
            || self.formats.objects != 2
        {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

/// Purpose keys of an unlocked vault. Dropped (zeroized) on lock.
pub struct VaultKeys {
    master: SecretKey,
    pub db: SecretKey,
    pub objects: SecretKey,
    pub index: SecretKey,
    pub backup: SecretKey,
}

impl VaultKeys {
    fn from_master(vault_id: &str, master: SecretKey) -> Result<Self> {
        let salt = vault_id.as_bytes();
        Ok(Self {
            db: master.derive(salt, b"lvjin/v1/sqlcipher")?,
            objects: master.derive(salt, b"lvjin/v1/objects")?,
            index: master.derive(salt, b"lvjin/v1/index-snapshot")?,
            backup: master.derive(salt, b"lvjin/v1/backup")?,
            master,
        })
    }
}

/// Displayed exactly once at creation; never stored or logged.
pub struct RecoveryCode(pub Zeroizing<String>);

fn encode_recovery(bytes: &[u8; 20]) -> Zeroizing<String> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut raw = Zeroizing::new(String::new());
    for byte in bytes {
        bits = (bits << 8) | u32::from(*byte);
        count += 8;
        while count >= 5 {
            count -= 5;
            raw.push(CROCKFORD[((bits >> count) & 31) as usize] as char);
        }
        bits &= (1 << count) - 1;
    }
    let mut grouped = Zeroizing::new(String::with_capacity(39));
    for (index, ch) in raw.chars().enumerate() {
        if index > 0 && index % 4 == 0 {
            grouped.push('-');
        }
        grouped.push(ch);
    }
    grouped
}

fn decode_recovery(text: &str) -> Result<Zeroizing<[u8; 20]>> {
    let mut bits = 0u64;
    let mut count = 0;
    let mut output = Zeroizing::new([0u8; 20]);
    let mut written = 0;
    for ch in text.chars() {
        let ch = match ch.to_ascii_uppercase() {
            ' ' | '-' => continue,
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        };
        let value = CROCKFORD.iter().position(|c| *c as char == ch).ok_or(Error::Denied)? as u64;
        bits = (bits << 5) | value;
        count += 5;
        if count >= 8 {
            count -= 8;
            if written >= 20 {
                return Err(Error::Denied);
            }
            output[written] = ((bits >> count) & 0xff) as u8;
            written += 1;
        }
    }
    if written != 20 {
        return Err(Error::Denied);
    }
    Ok(output)
}

fn recovery_kek(code: &[u8; 20], salt: &[u8], vault_id: &str) -> Result<SecretKey> {
    // The code carries 160 random bits, so HKDF (not a slow KDF) is sufficient.
    SecretKey::from_bytes({
        let mut ikm = [0u8; 32];
        ikm[..20].copy_from_slice(code);
        ikm
    })
    .derive(salt, format!("lvjin/recovery-kek/v1|{vault_id}").as_bytes())
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}
fn unb64(text: &str) -> Result<Vec<u8>> {
    STANDARD.decode(text).map_err(|_| Error::Integrity)
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Create a new vault directory with header and wrapped keys.
pub fn create(directory: &Path, passphrase: &[u8], params: KdfParams) -> Result<(VaultHeader, VaultKeys, RecoveryCode)> {
    if directory.exists() {
        return Err(Error::Conflict);
    }
    let vault_id = format!("{:x}", uuid::Uuid::new_v4().simple());
    let salt = random_bytes::<16>();
    let recovery_salt = random_bytes::<16>();
    let recovery = Zeroizing::new(random_bytes::<20>());
    let master = SecretKey::generate();
    let mut header = VaultHeader {
        schema_version: SCHEMA.into(),
        vault_id: vault_id.clone(),
        created_at: now_rfc3339(),
        kdf: KdfRecord { algorithm: "argon2id-v19".into(), params, salt: b64(&salt) },
        passphrase_wrap: String::new(),
        recovery_salt: b64(&recovery_salt),
        recovery_wrap: String::new(),
        formats: Formats { database: 1, objects: 2, index: 1 },
    };
    header.validate()?;
    let kek = passphrase_kek(passphrase, &salt, params)?;
    header.passphrase_wrap = b64(&kek.seal(&master.0, &header.passphrase_aad())?);
    let rkek = recovery_kek(&recovery, &recovery_salt, &vault_id)?;
    header.recovery_wrap = b64(&rkek.seal(&master.0, &header.recovery_aad())?);
    std::fs::create_dir_all(directory.join(OBJECTS_DIR))?;
    write_header(directory, &header)?;
    let code = RecoveryCode(encode_recovery(&recovery));
    Ok((header, VaultKeys::from_master(&vault_id, master)?, code))
}

fn unwrap_master(wrapped: &str, kek: &SecretKey, aad: &[u8]) -> Result<SecretKey> {
    let plain = Zeroizing::new(kek.open(&unb64(wrapped)?, aad).map_err(|_| Error::Denied)?);
    let bytes: [u8; 32] = plain.as_slice().try_into().map_err(|_| Error::Integrity)?;
    Ok(SecretKey::from_bytes(bytes))
}

pub fn unlock_with_passphrase(header: &VaultHeader, passphrase: &[u8]) -> Result<VaultKeys> {
    header.validate()?;
    let salt: [u8; 16] = unb64(&header.kdf.salt)?.try_into().map_err(|_| Error::Integrity)?;
    let kek = passphrase_kek(passphrase, &salt, header.kdf.params)?;
    let master = unwrap_master(&header.passphrase_wrap, &kek, &header.passphrase_aad())?;
    VaultKeys::from_master(&header.vault_id, master)
}

pub fn unlock_with_recovery(header: &VaultHeader, code: &str) -> Result<VaultKeys> {
    header.validate()?;
    let bytes = decode_recovery(code)?;
    let kek = recovery_kek(&bytes, &unb64(&header.recovery_salt)?, &header.vault_id)?;
    let master = unwrap_master(&header.recovery_wrap, &kek, &header.recovery_aad())?;
    VaultKeys::from_master(&header.vault_id, master)
}

/// Re-wrap the unchanged master key under a new passphrase (and parameters).
pub fn rewrap_passphrase(directory: &Path, header: &VaultHeader, keys: &VaultKeys, passphrase: &[u8], params: KdfParams) -> Result<VaultHeader> {
    let mut updated = header.clone();
    let salt = random_bytes::<16>();
    updated.kdf = KdfRecord { algorithm: "argon2id-v19".into(), params, salt: b64(&salt) };
    updated.validate()?;
    let kek = passphrase_kek(passphrase, &salt, params)?;
    updated.passphrase_wrap = b64(&kek.seal(&keys.master.0, &updated.passphrase_aad())?);
    write_header(directory, &updated)?;
    Ok(updated)
}

/// Copy of the wrapping header for an encrypted backup (master key unchanged).
pub fn backup_header(header: &VaultHeader) -> VaultHeader {
    header.clone()
}

pub fn read_header(directory: &Path) -> Result<VaultHeader> {
    let header: VaultHeader = serde_json::from_slice(&std::fs::read(directory.join(HEADER_FILE))?)?;
    header.validate()?;
    Ok(header)
}

/// Atomic replace: write a sibling temporary file, then rename over the header.
pub fn write_header(directory: &Path, header: &VaultHeader) -> Result<()> {
    let target = directory.join(HEADER_FILE);
    let temporary = directory.join(format!("{HEADER_FILE}.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(header)?)?;
    std::fs::rename(&temporary, &target)?;
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct VaultSummary {
    pub handle: String,
    pub vault_id: String,
    pub created_at: String,
}

/// Vault directories under `<data_root>/vaults`. Only public header fields.
pub fn list(vaults_root: &Path) -> Result<Vec<VaultSummary>> {
    let mut output = vec![];
    if !vaults_root.is_dir() {
        return Ok(output);
    }
    for entry in std::fs::read_dir(vaults_root)? {
        let entry = entry?;
        let handle = entry.file_name().to_string_lossy().to_string();
        if !entry.file_type()?.is_dir() || !handle.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            continue;
        }
        if let Ok(header) = read_header(&entry.path()) {
            output.push(VaultSummary { handle, vault_id: header.vault_id, created_at: header.created_at });
        }
    }
    output.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.handle.cmp(&b.handle)));
    Ok(output)
}

/// Resolve a vault handle to its directory without allowing traversal.
pub fn directory_for(vaults_root: &Path, handle: &str) -> Result<PathBuf> {
    if handle.is_empty() || handle.len() > 80 || !handle.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(Error::Contract);
    }
    Ok(vaults_root.join(handle))
}

#[cfg(feature = "m0-harness")]
pub mod harness {
    /// Fast, floor-compliant parameters for synthetic tests only.
    pub const TEST_KDF: crate::crypto::KdfParams = crate::crypto::KdfParams { memory_kib: 19456, iterations: 2, parallelism: 1 };
    pub fn roundtrip_recovery(bytes: &[u8; 20]) -> String {
        super::encode_recovery(bytes).to_string()
    }
}
