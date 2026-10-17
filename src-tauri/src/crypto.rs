use crate::{Error, Result};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::{RngCore, rngs::OsRng};
use zeroize::{Zeroize, ZeroizeOnDrop};

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
