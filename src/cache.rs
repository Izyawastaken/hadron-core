//! Filesystem artifact cache. Each read verifies content; writes are atomic.
use crate::{Error, Result, model::Hash};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// Whether optional cache failures abort a consumer operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CachePolicy {
    /// Preserve errors, including corruption and failed writes.
    #[default]
    Strict,
    /// Refetch corrupt entries and tolerate local cache I/O failures.
    Recover,
}

/// A caller-owned cache directory. Do not share it with untrusted writers.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
}
impl Cache {
    /// Create/open a cache directory, rejecting a symlink as its root.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        std::fs::create_dir_all(root.as_ref())?;
        if std::fs::symlink_metadata(root.as_ref())?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Invalid("cache root cannot be a symlink".into()));
        }
        Ok(Self {
            root: std::fs::canonicalize(root)?,
        })
    }
    /// Read verified content or return a miss. Corrupt cache entries return an error.
    pub fn get(&self, hash: &Hash, expected_size: u64, maximum: u64) -> Result<Option<Vec<u8>>> {
        if expected_size > maximum {
            return Err(Error::Limit("cache bytes"));
        }
        let path = self.root.join(hash.as_str());
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != expected_size {
            return Err(Error::Integrity);
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(maximum.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected_size {
            return Err(Error::Integrity);
        }
        hash.verify(&bytes)?;
        Ok(Some(bytes))
    }
    /// Verify then persist without overwriting another writer. No eviction is automatic.
    pub fn put(&self, hash: &Hash, bytes: &[u8]) -> Result<()> {
        hash.verify(bytes)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        match temp.persist_noclobber(self.root.join(hash.as_str())) {
            Ok(_) => Ok(()),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.get(hash, bytes.len() as u64, bytes.len() as u64)?
                    .ok_or(Error::Integrity)?;
                Ok(())
            }
            Err(e) => Err(e.error.into()),
        }
    }
    /// Remove one content key, never traversing directories. Missing keys are fine.
    /// Callers must coordinate eviction with consumers using this trusted directory.
    pub fn remove(&self, hash: &Hash) -> Result<()> {
        match std::fs::remove_file(self.root.join(hash.as_str())) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    /// Count regular, canonical hash-named entries; ignores temporary/foreign files.
    pub fn size(&self) -> Result<u64> {
        self.entries()?.iter().try_fold(0u64, |n, (_, size)| {
            n.checked_add(*size).ok_or(Error::Limit("cache size"))
        })
    }
    /// Explicit deterministic eviction to a byte budget. Returns bytes removed.
    /// Coordinate with other writers; this is not a cross-process quota.
    pub fn prune(&self, maximum: u64) -> Result<u64> {
        let entries = self.entries()?;
        let mut total = self.size()?;
        let mut removed = 0u64;
        for (hash, size) in entries {
            if total <= maximum {
                break;
            }
            self.remove(&hash)?;
            total = total.saturating_sub(size);
            removed = removed.saturating_add(size);
        }
        Ok(removed)
    }
    fn entries(&self) -> Result<Vec<(Hash, u64)>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let Ok(hash) = Hash::parse(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_file() && !meta.file_type().is_symlink() {
                entries.push((hash, meta.len()));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }
}
