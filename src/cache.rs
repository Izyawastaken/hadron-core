//! Filesystem artifact cache. Each read verifies content; writes are atomic.
use crate::{Error, Result, model::Hash};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

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
}
