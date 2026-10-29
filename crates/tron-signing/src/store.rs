//! Durable signer state: one file per signed transaction (idempotency) and the spend log
//! behind the policy's rolling limits. Both survive a restart; without them a restart
//! would reset the daily caps and a retry after a lost broadcast reply would sign a
//! second, different transaction for the same payout.
//!
//! Files are written to a temp name, fsynced and renamed, then the directory is fsynced,
//! so a record is either fully on disk or absent. A `LOCK` file held for the lifetime of
//! the process keeps two signers from sharing one state directory.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tron::UnsignedTx;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("state directory {0}: {1}")]
    Io(String, std::io::Error),
    #[error("state directory {0} is in use by another signer")]
    Locked(String),
    #[error("corrupt state file {0}: {1}")]
    Corrupt(String, String),
}

/// A transaction we signed for one reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Hash of the request that produced it; a repeat with other parameters is refused.
    pub fingerprint: String,
    pub tx: UnsignedTx,
    pub signature: String,
    pub expiration_ms: i64,
    /// The response body (`TransferResult` / `DelegationResult`).
    pub result: serde_json::Value,
    /// The node accepted the broadcast at least once.
    pub broadcast: bool,
}

pub struct Store {
    dir: Option<PathBuf>,
    /// In-memory mode only (tests, dev without `SIGNER_STATE_DIR`).
    mem: Mutex<HashMap<String, Vec<u8>>>,
    _lock: Option<File>,
}

impl Store {
    /// Nothing survives a restart. Dev and tests only.
    pub fn memory() -> Self {
        Self {
            dir: None,
            mem: Mutex::new(HashMap::new()),
            _lock: None,
        }
    }

    pub fn open(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        let name = dir.display().to_string();
        let io = |e| StoreError::Io(name.clone(), e);
        fs::create_dir_all(dir.join("tx")).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        // Make the freshly created directories themselves durable, not only the files in
        // them: a crash right after the first record must not lose the `tx` entry.
        for d in [Some(dir.as_path()), dir.parent()].into_iter().flatten() {
            if !d.as_os_str().is_empty() {
                File::open(d).and_then(|f| f.sync_all()).map_err(io)?;
            }
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("LOCK"))
            .map_err(io)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(StoreError::Locked(name)),
            Err(fs::TryLockError::Error(e)) => return Err(io(e)),
        }
        Ok(Self {
            dir: Some(dir),
            mem: Mutex::new(HashMap::new()),
            _lock: Some(lock),
        })
    }

    pub fn is_durable(&self) -> bool {
        self.dir.is_some()
    }

    fn tx_path(dir: &Path, key: &str) -> PathBuf {
        dir.join("tx").join(format!(
            "{}.json",
            hex::encode(Sha256::digest(key.as_bytes()))
        ))
    }

    pub fn get_record(&self, key: &str) -> Result<Option<Record>, StoreError> {
        match &self.dir {
            Some(dir) => read_json(&Self::tx_path(dir, key)),
            None => self
                .mem
                .lock()
                .unwrap()
                .get(&format!("tx:{key}"))
                .map(|b| {
                    serde_json::from_slice(b)
                        .map_err(|e| StoreError::Corrupt(key.to_owned(), e.to_string()))
                })
                .transpose(),
        }
    }

    pub fn put_record(&self, key: &str, record: &Record) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec_pretty(record).expect("record serializes");
        match &self.dir {
            Some(dir) => write_atomic(&Self::tx_path(dir, key), &bytes),
            None => {
                self.mem.lock().unwrap().insert(format!("tx:{key}"), bytes);
                Ok(())
            }
        }
    }

    pub fn remove_record(&self, key: &str) -> Result<(), StoreError> {
        match &self.dir {
            Some(dir) => {
                let path = Self::tx_path(dir, key);
                let io = |e| StoreError::Io(path.display().to_string(), e);
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(io(e)),
                }
                File::open(dir.join("tx"))
                    .and_then(|d| d.sync_all())
                    .map_err(io)
            }
            None => {
                self.mem.lock().unwrap().remove(&format!("tx:{key}"));
                Ok(())
            }
        }
    }

    pub fn get_blob<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, StoreError> {
        match &self.dir {
            Some(dir) => read_json(&dir.join(name)),
            None => self
                .mem
                .lock()
                .unwrap()
                .get(name)
                .map(|b| {
                    serde_json::from_slice(b)
                        .map_err(|e| StoreError::Corrupt(name.to_owned(), e.to_string()))
                })
                .transpose(),
        }
    }

    pub fn put_blob<T: Serialize>(&self, name: &str, value: &T) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(value).expect("state serializes");
        match &self.dir {
            Some(dir) => write_atomic(&dir.join(name), &bytes),
            None => {
                self.mem.lock().unwrap().insert(name.to_owned(), bytes);
                Ok(())
            }
        }
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| StoreError::Corrupt(path.display().to_string(), e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Io(path.display().to_string(), e)),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let name = path.display().to_string();
    let io = |e| StoreError::Io(name.clone(), e);
    let tmp = path.with_extension("tmp");
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(io)?;
    f.write_all(bytes).map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    fs::rename(&tmp, path).map_err(io)?;
    if let Some(parent) = path.parent() {
        File::open(parent).and_then(|d| d.sync_all()).map_err(io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gtron-signer-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn record() -> Record {
        Record {
            fingerprint: "f".into(),
            tx: UnsignedTx {
                tx_id: "aa".into(),
                raw_data_hex: "0a02".into(),
                rest: Default::default(),
            },
            signature: "00".into(),
            expiration_ms: 1,
            result: serde_json::json!({ "tx_id": "aa" }),
            broadcast: false,
        }
    }

    #[test]
    fn records_survive_reopen_and_dir_is_exclusive() {
        let dir = tmpdir("store");
        {
            let s = Store::open(&dir).unwrap();
            assert!(matches!(Store::open(&dir), Err(StoreError::Locked(_))));
            assert!(s.get_record("withdrawal:1").unwrap().is_none());
            s.put_record("withdrawal:1", &record()).unwrap();
            s.put_blob("spend.json", &vec![1, 2, 3]).unwrap();
        }
        let s = Store::open(&dir).unwrap();
        let r = s.get_record("withdrawal:1").unwrap().unwrap();
        assert_eq!(r.tx.tx_id, "aa");
        assert_eq!(
            s.get_blob::<Vec<i32>>("spend.json").unwrap(),
            Some(vec![1, 2, 3])
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
