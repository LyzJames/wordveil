use crate::{Error, Result};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use rand::{RngCore, rngs::OsRng};
use sha2::Sha256;
use std::io::{Read, Write};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretKey(pub(crate) [u8; 32]);

impl SecretKey {
    pub fn generate() -> Self {
        let mut bytes = [0; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }
    /// The caller is trusted key-management code, never a worker/Skill payload.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Purpose-separated subkey (HKDF-SHA256). `info` names purpose and context.
    pub fn derive(&self, salt: &[u8], info: &[u8]) -> Result<SecretKey> {
        let expander = Hkdf::<Sha256>::new(Some(salt), &self.0);
        let mut output = [0u8; 32];
        expander.expand(info, &mut output).map_err(|_| Error::Crypto)?;
        Ok(SecretKey(output))
    }

    /// Raw SQLCipher key literal content. Lives only in a zeroizing buffer.
    pub(crate) fn hex(&self) -> Zeroizing<String> {
        let mut text = Zeroizing::new(String::with_capacity(64));
        for byte in self.0 {
            text.push_str(&format!("{byte:02x}"));
        }
        text
    }

    pub fn seal(&self, plaintext: &[u8], associated_data: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(&self.0).map_err(|_| Error::Crypto)?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: associated_data,
                },
            )
            .map_err(|_| Error::Crypto)?;
        let mut output = b"LVJ1".to_vec();
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&encrypted);
        Ok(output)
    }

    pub fn open(&self, envelope: &[u8], associated_data: &[u8]) -> Result<Vec<u8>> {
        if envelope.len() < 32 || &envelope[..4] != b"LVJ1" {
            return Err(Error::Crypto);
        }
        Aes256Gcm::new_from_slice(&self.0)
            .map_err(|_| Error::Crypto)?
            .decrypt(
                Nonce::from_slice(&envelope[4..16]),
                Payload {
                    msg: &envelope[16..],
                    aad: associated_data,
                },
            )
            .map_err(|_| Error::Crypto)
    }
}

/// Argon2id cost parameters for wrapping a vault master key with a passphrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl KdfParams {
    /// Design 12.2 starting point (64 MiB, 3 passes); calibrated on the target.
    pub const DESIGN_DEFAULT: KdfParams = KdfParams { memory_kib: 65536, iterations: 3, parallelism: 1 };

    /// OWASP Password Storage Cheat Sheet floors: 19 MiB/t=2 or 46 MiB/t=1.
    pub fn meets_floor(&self) -> bool {
        self.parallelism >= 1
            && self.parallelism <= 4
            && self.memory_kib <= 4 * 1024 * 1024
            && ((self.memory_kib >= 19456 && self.iterations >= 2) || (self.memory_kib >= 47104 && self.iterations >= 1))
    }
}

/// Derive a key-encryption key from a passphrase. Weak parameters are refused.
pub fn passphrase_kek(passphrase: &[u8], salt: &[u8; 16], params: KdfParams) -> Result<SecretKey> {
    if !params.meets_floor() || passphrase.is_empty() {
        return Err(Error::Crypto);
    }
    let cost = Params::new(params.memory_kib, params.iterations, params.parallelism, Some(32)).map_err(|_| Error::Crypto)?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, cost);
    let mut output = [0u8; 32];
    argon.hash_password_into(passphrase, salt, &mut output).map_err(|_| Error::Crypto)?;
    Ok(SecretKey(output))
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

const STREAM_MAGIC: &[u8; 4] = b"LVJ2";
pub const STREAM_CHUNK: usize = 1024 * 1024;
const STREAM_HEADER: usize = 4 + 1 + 3 + 32 + 4 + 8;
pub const STREAM_MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

fn stream_subkey(key: &SecretKey, salt: &[u8; 32], context: &[u8]) -> Result<Aes256Gcm> {
    let mut info = b"lvjin/object-stream/v2|".to_vec();
    info.extend_from_slice(context);
    let subkey = key.derive(salt, &info)?;
    Aes256Gcm::new_from_slice(&subkey.0).map_err(|_| Error::Crypto)
}

fn chunk_nonce(index: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&index.to_be_bytes());
    nonce
}

fn chunk_aad(context: &[u8], index: u64, last: bool) -> Vec<u8> {
    let mut aad = context.to_vec();
    aad.extend_from_slice(&index.to_be_bytes());
    aad.push(u8::from(last));
    aad
}

/// Chunked AEAD envelope (LVJ2) for blobs of any size.
///
/// A fresh 256-bit salt per object derives a unique AES-256-GCM subkey with
/// HKDF, so chunk counters can serve as nonces without reuse. The context
/// (vault, object id, kind) and the final-chunk flag are authenticated, which
/// rejects swapping, reordering and truncation. Plaintext of a chunk is only
/// released after that chunk authenticates.
pub fn seal_stream<R: Read, W: Write>(key: &SecretKey, context: &[u8], mut reader: R, total: u64, mut writer: W) -> Result<()> {
    if total > STREAM_MAX_BYTES {
        return Err(Error::Contract);
    }
    let salt = random_bytes::<32>();
    let cipher = stream_subkey(key, &salt, context)?;
    let mut header = Vec::with_capacity(STREAM_HEADER);
    header.extend_from_slice(STREAM_MAGIC);
    header.extend_from_slice(&[2, 0, 0, 0]);
    header.extend_from_slice(&salt);
    header.extend_from_slice(&(STREAM_CHUNK as u32).to_le_bytes());
    header.extend_from_slice(&total.to_le_bytes());
    writer.write_all(&header)?;
    let chunks = if total == 0 { 1 } else { total.div_ceil(STREAM_CHUNK as u64) };
    let mut buffer = Zeroizing::new(vec![0u8; STREAM_CHUNK]);
    for index in 0..chunks {
        let size = if total == 0 { 0 } else { ((total - index * STREAM_CHUNK as u64).min(STREAM_CHUNK as u64)) as usize };
        reader.read_exact(&mut buffer[..size])?;
        let last = index + 1 == chunks;
        let sealed = cipher
            .encrypt(Nonce::from_slice(&chunk_nonce(index)), Payload { msg: &buffer[..size], aad: &chunk_aad(context, index, last) })
            .map_err(|_| Error::Crypto)?;
        writer.write_all(&sealed)?;
    }
    // A reader that yields more than declared means the caller lied about size.
    let mut probe = [0u8; 1];
    if reader.read(&mut probe)? != 0 {
        return Err(Error::Integrity);
    }
    writer.flush()?;
    Ok(())
}

pub fn open_stream<R: Read, W: Write>(key: &SecretKey, context: &[u8], mut reader: R, mut writer: W) -> Result<u64> {
    let mut header = [0u8; STREAM_HEADER];
    reader.read_exact(&mut header).map_err(|_| Error::Crypto)?;
    if &header[..4] != STREAM_MAGIC || header[4] != 2 {
        return Err(Error::Crypto);
    }
    let salt: [u8; 32] = header[8..40].try_into().map_err(|_| Error::Crypto)?;
    let chunk_size = u32::from_le_bytes(header[40..44].try_into().map_err(|_| Error::Crypto)?) as usize;
    let total = u64::from_le_bytes(header[44..52].try_into().map_err(|_| Error::Crypto)?);
    if chunk_size != STREAM_CHUNK || total > STREAM_MAX_BYTES {
        return Err(Error::Crypto);
    }
    let cipher = stream_subkey(key, &salt, context)?;
    let chunks = if total == 0 { 1 } else { total.div_ceil(STREAM_CHUNK as u64) };
    let mut sealed = vec![0u8; STREAM_CHUNK + 16];
    for index in 0..chunks {
        let size = if total == 0 { 0 } else { ((total - index * STREAM_CHUNK as u64).min(STREAM_CHUNK as u64)) as usize };
        reader.read_exact(&mut sealed[..size + 16]).map_err(|_| Error::Crypto)?;
        let last = index + 1 == chunks;
        let plain = Zeroizing::new(
            cipher
                .decrypt(Nonce::from_slice(&chunk_nonce(index)), Payload { msg: &sealed[..size + 16], aad: &chunk_aad(context, index, last) })
                .map_err(|_| Error::Crypto)?,
        );
        writer.write_all(&plain)?;
    }
    let mut trailing = [0u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(Error::Crypto);
    }
    Ok(total)
}

pub fn seal_bytes(key: &SecretKey, context: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(plaintext.len() + STREAM_HEADER + 16 * (plaintext.len() / STREAM_CHUNK + 1));
    seal_stream(key, context, plaintext, plaintext.len() as u64, &mut output)?;
    Ok(output)
}

pub fn open_bytes(key: &SecretKey, context: &[u8], envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let mut output = Zeroizing::new(Vec::new());
    open_stream(key, context, envelope, &mut *output)?;
    Ok(output)
}
