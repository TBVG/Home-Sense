// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local-only encrypted storage. No network code exists in this crate.
//!
//! * Each house is one file `<id>.house`, ChaCha20-Poly1305 encrypted.
//! * Each house has its own random 256-bit key stored in the OS keychain (`keyring`).
//! * If the keychain is unavailable, nothing is written. There is no plaintext fallback.

pub mod model;
pub use model::*;

use chacha20poly1305::{aead::{Aead, KeyInit}, ChaCha20Poly1305, Key, Nonce};
use rand::{rngs::OsRng, RngCore};
use std::{fs, io::Write, path::{Path, PathBuf}};

const MAGIC_HOUSE: &[u8; 4] = b"HAH1";
const MAGIC_BACKUP: &[u8; 4] = b"HAB1";
const KEYRING_SERVICE: &str = "home-awareness";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("OS keychain unavailable: {0}")]
    Keychain(String),
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("data could not be decrypted (wrong key, wrong passphrase, or corrupted file)")]
    Decrypt,
    #[error("data format error: {0}")]
    Format(String),
}

/// Where the 32-byte key for a house comes from. The OS keychain in the app.
pub trait KeyProvider: Send + Sync {
    fn get_or_create(&self, house_id: &str) -> Result<[u8; 32], StoreError>;
    fn get(&self, house_id: &str) -> Result<Option<[u8; 32]>, StoreError>;
    fn delete(&self, house_id: &str) -> Result<(), StoreError>;
}

pub struct KeychainKeys;

impl KeychainKeys {
    fn entry(id: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("house-{id}")).map_err(|e| StoreError::Keychain(e.to_string()))
    }
    /// Check that the keychain works at all, without touching any house.
    pub fn probe() -> Result<(), StoreError> {
        let e = keyring::Entry::new(KEYRING_SERVICE, "probe").map_err(|e| StoreError::Keychain(e.to_string()))?;
        match e.get_password() {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(StoreError::Keychain(err.to_string())),
        }
    }
}

fn parse_key(s: &str) -> Result<[u8; 32], StoreError> {
    let v = hex::decode(s).map_err(|_| StoreError::Keychain("stored key is malformed".into()))?;
    v.try_into().map_err(|_| StoreError::Keychain("stored key has wrong length".into()))
}

impl KeyProvider for KeychainKeys {
    fn get(&self, id: &str) -> Result<Option<[u8; 32]>, StoreError> {
        match Self::entry(id)?.get_password() {
            Ok(s) => Ok(Some(parse_key(&s)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }
    fn get_or_create(&self, id: &str) -> Result<[u8; 32], StoreError> {
        if let Some(k) = self.get(id)? {
            return Ok(k);
        }
        let mut k = [0u8; 32];
        OsRng.fill_bytes(&mut k);
        Self::entry(id)?.set_password(&hex::encode(k)).map_err(|e| StoreError::Keychain(e.to_string()))?;
        Ok(k)
    }
    fn delete(&self, id: &str) -> Result<(), StoreError> {
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Keychain(e.to_string())),
        }
    }
}

fn seal(key: &[u8; 32], magic: &[u8; 4], extra: &[u8], plain: &[u8]) -> Result<Vec<u8>, StoreError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), plain).map_err(|_| StoreError::Decrypt)?;
    let mut out = Vec::with_capacity(4 + extra.len() + 12 + ct.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(extra);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open(key: &[u8; 32], bytes: &[u8], extra_len: usize) -> Result<Vec<u8>, StoreError> {
    let start = 4 + extra_len;
    if bytes.len() < start + 12 + 16 {
        return Err(StoreError::Format("file too short".into()));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher.decrypt(Nonce::from_slice(&bytes[start..start + 12]), &bytes[start + 12..]).map_err(|_| StoreError::Decrypt)
}

fn derive(pass: &str, salt: &[u8]) -> Result<[u8; 32], StoreError> {
    let mut k = [0u8; 32];
    argon2::Argon2::default().hash_password_into(pass.as_bytes(), salt, &mut k).map_err(|e| StoreError::Format(e.to_string()))?;
    Ok(k)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}

pub fn new_id() -> String {
    let mut b = [0u8; 8];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub struct Store {
    dir: PathBuf,
    keys: Box<dyn KeyProvider>,
}

impl Store {
    /// Open the store in the user's per-OS data directory, with keys in the OS keychain.
    pub fn open_default() -> Result<Self, StoreError> {
        KeychainKeys::probe()?;
        let dir = dirs::data_local_dir().ok_or_else(|| StoreError::Format("no data directory".into()))?.join("home-awareness");
        Self::open_in(dir, Box::new(KeychainKeys))
    }

    pub fn open_in(dir: PathBuf, keys: Box<dyn KeyProvider>) -> Result<Self, StoreError> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, keys })
    }

    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    fn house_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.house"))
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("app_state.json")
    }

    pub fn load_state(&self) -> AppState {
        fs::read(self.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_state(&self, s: &AppState) -> Result<(), StoreError> {
        write_atomic(&self.state_path(), &serde_json::to_vec(s).map_err(|e| StoreError::Format(e.to_string()))?)
    }

    /// (id, name) of every readable house.
    pub fn list_houses(&self) -> Vec<(String, String)> {
        let mut v = vec![];
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("house") {
                    if let Some(id) = p.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(h) = self.load_house(id) {
                            v.push((h.id, h.name));
                        }
                    }
                }
            }
        }
        v.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        v
    }

    pub fn create_house(&self, name: &str) -> Result<House, StoreError> {
        let h = House::new(new_id(), name.trim().to_string());
        self.save_house(&h)?;
        Ok(h)
    }

    pub fn save_house(&self, h: &House) -> Result<(), StoreError> {
        let key = self.keys.get_or_create(&h.id)?;
        let json = serde_json::to_vec(h).map_err(|e| StoreError::Format(e.to_string()))?;
        write_atomic(&self.house_path(&h.id), &seal(&key, MAGIC_HOUSE, &[], &json)?)
    }

    pub fn load_house(&self, id: &str) -> Result<House, StoreError> {
        let bytes = fs::read(self.house_path(id))?;
        if bytes.len() < 4 || &bytes[..4] != MAGIC_HOUSE {
            return Err(StoreError::Format("not a house file".into()));
        }
        let key = self.keys.get(id)?.ok_or(StoreError::Decrypt)?;
        serde_json::from_slice(&open(&key, &bytes, 0)?).map_err(|e| StoreError::Format(e.to_string()))
    }

    pub fn delete_house(&self, id: &str) -> Result<(), StoreError> {
        let p = self.house_path(id);
        if p.exists() {
            fs::remove_file(p)?;
        }
        self.keys.delete(id)
    }

    pub fn delete_all(&self) -> Result<(), StoreError> {
        for (id, _) in self.list_houses() {
            self.delete_house(&id)?;
        }
        // also remove files whose key is gone and thus unreadable
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if matches!(p.extension().and_then(|s| s.to_str()), Some("house") | Some("tmp")) {
                    let _ = fs::remove_file(p);
                }
            }
        }
        let _ = fs::remove_file(self.state_path());
        Ok(())
    }

    /// Export one house, encrypted with a key derived from `passphrase`.
    pub fn export_backup(&self, id: &str, passphrase: &str) -> Result<Vec<u8>, StoreError> {
        let h = self.load_house(id)?;
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let key = derive(passphrase, &salt)?;
        seal(&key, MAGIC_BACKUP, &salt, &serde_json::to_vec(&h).map_err(|e| StoreError::Format(e.to_string()))?)
    }

    /// Import a backup as a new house (new id, new keychain key).
    pub fn import_backup(&self, bytes: &[u8], passphrase: &str) -> Result<House, StoreError> {
        if bytes.len() < 4 + 16 || &bytes[..4] != MAGIC_BACKUP {
            return Err(StoreError::Format("not a backup file".into()));
        }
        let key = derive(passphrase, &bytes[4..20])?;
        let mut h: House = serde_json::from_slice(&open(&key, bytes, 16)?).map_err(|e| StoreError::Format(e.to_string()))?;
        h.id = new_id();
        self.save_house(&h)?;
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MATH CHECK: seal then open returns the same bytes, and a wrong key fails.
    /// Uses arbitrary literal bytes, not app data; never touches the UI or a real store.
    #[test]
    fn math_check_aead_roundtrip() {
        let k = [7u8; 32];
        let sealed = seal(&k, MAGIC_HOUSE, &[], b"abc").unwrap();
        assert_eq!(open(&k, &sealed, 0).unwrap(), b"abc");
        assert!(open(&[8u8; 32], &sealed, 0).is_err());
    }
}
