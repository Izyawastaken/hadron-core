//! Reproducible ZSTD-9 manifests and regular-file-only configuration archives.
use crate::{
    Error, Result,
    model::*,
    validation::{self, Limits},
};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
};

/// Compressed artifact plus its decoded integrity metadata.
#[derive(Clone, Debug)]
pub struct Encoded {
    /// Exact bytes to upload; Zstandard frame.
    pub bytes: Vec<u8>,
    /// Compressed SHA-256.
    pub sha256: Hash,
    /// Decoded byte count.
    pub decoded_size: u64,
    /// Decoded SHA-256.
    pub decoded_sha256: Hash,
}
impl Encoded {
    /// Bind a successful GitHub asset upload to this content.
    pub fn asset(&self, id: u64, name: String) -> Asset {
        Asset {
            id,
            name,
            size: self.bytes.len() as u64,
            sha256: self.sha256.clone(),
            decoded_size: self.decoded_size,
            decoded_sha256: self.decoded_sha256.clone(),
        }
    }
}
/// Locally prepared pack. It does not contain executable hooks or remote credentials.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// Validated inventory.
    pub manifest: Manifest,
    /// Zstandard manifest payload.
    pub manifest_artifact: Encoded,
    /// Optional deterministic configuration tar frame.
    pub config_artifact: Option<Encoded>,
}
/// Compress bounded bytes at Zstandard level 9. CPU-bound; async callers must offload.
pub fn encode(bytes: &[u8], limits: &Limits) -> Result<Encoded> {
    if bytes.len() as u64 > limits.decoded_bytes {
        return Err(Error::Limit("decoded bytes"));
    }
    let compressed = zstd::stream::encode_all(bytes, 9).map_err(|_| Error::Codec)?;
    if compressed.len() as u64 > limits.compressed_bytes {
        return Err(Error::Limit("compressed bytes"));
    }
    Ok(Encoded {
        sha256: Hash::of(&compressed),
        bytes: compressed,
        decoded_size: bytes.len() as u64,
        decoded_sha256: Hash::of(bytes),
    })
}
/// Bounded Zstandard decode with size, window and both hash checks.
pub fn decode(bytes: &[u8], asset: &Asset, limits: &Limits) -> Result<Vec<u8>> {
    validation::asset(asset, limits)?;
    verify(bytes, asset)?;
    let mut decoder =
        zstd::stream::read::Decoder::new(Cursor::new(bytes)).map_err(|_| Error::Codec)?;
    decoder
        .window_log_max(limits.window_log)
        .map_err(|_| Error::Codec)?;
    let mut out = Vec::new();
    decoder
        .take(asset.decoded_size.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|_| Error::Codec)?;
    if out.len() as u64 != asset.decoded_size {
        return Err(Error::Integrity);
    }
    asset.decoded_sha256.verify(&out)?;
    Ok(out)
}
/// Verify downloaded raw bytes against asset metadata before caching.
pub fn verify(bytes: &[u8], asset: &Asset) -> Result<()> {
    if bytes.len() as u64 != asset.size {
        return Err(Error::Integrity);
    }
    asset.sha256.verify(bytes)
}
/// Build from exact caller-provided config bytes, preserving all bytes and comments.
/// Keys are instance-relative paths under `config/`. No local directory is traversed.
pub fn prepare(
    mut manifest: Manifest,
    configs: &BTreeMap<String, Vec<u8>>,
    limits: &Limits,
) -> Result<Prepared> {
    if configs.len() > limits.entries {
        return Err(Error::Limit("config entries"));
    }
    manifest.configs = configs
        .iter()
        .map(|(path, bytes)| {
            (
                path.clone(),
                FileSpec {
                    path: path.clone(),
                    size: bytes.len() as u64,
                    sha256: Hash::of(bytes),
                },
            )
        })
        .collect();
    validation::manifest(&manifest, limits)?;
    let total = configs.values().try_fold(0u64, |n, v| {
        n.checked_add(v.len() as u64)
            .ok_or(Error::Limit("config bytes"))
    })?;
    if total > limits.decoded_bytes {
        return Err(Error::Limit("config bytes"));
    }
    let config_artifact = if configs.is_empty() {
        None
    } else {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes) in configs {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, path, bytes.as_slice())
                .map_err(|_| Error::Codec)?;
        }
        Some(encode(
            &builder.into_inner().map_err(|_| Error::Codec)?,
            limits,
        )?)
    };
    let manifest_artifact = encode(&serde_json::to_vec(&manifest)?, limits)?;
    Ok(Prepared {
        manifest,
        manifest_artifact,
        config_artifact,
    })
}
/// Decode configs to validated in-memory files, without extracting paths to disk.
/// The launcher must resolve local user edits and avoid symlinks when writing files.
pub fn configs(
    bytes: &[u8],
    asset: &Asset,
    manifest: &Manifest,
    limits: &Limits,
) -> Result<BTreeMap<String, Vec<u8>>> {
    validation::manifest(manifest, limits)?;
    let decoded = decode(bytes, asset, limits)?;
    let mut archive = tar::Archive::new(Cursor::new(decoded));
    let mut files = BTreeMap::new();
    for entry in archive.entries().map_err(|_| Error::Codec)? {
        if files.len() >= limits.entries {
            return Err(Error::Limit("archive entries"));
        }
        let mut entry = entry.map_err(|_| Error::Codec)?;
        if !entry.header().entry_type().is_file() {
            return Err(Error::Invalid("non-regular archive entry".into()));
        }
        let path = entry
            .path()
            .map_err(|_| Error::Codec)?
            .to_str()
            .ok_or_else(|| Error::Invalid("non-UTF8 archive path".into()))?
            .to_owned();
        validation::path(&path)?;
        let expected = manifest.configs.get(&path).ok_or(Error::Integrity)?;
        if entry.size() != expected.size
            || entry.size() > limits.file_bytes
            || files.contains_key(&path)
        {
            return Err(Error::Integrity);
        }
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(expected.size + 1)
            .read_to_end(&mut data)
            .map_err(|_| Error::Codec)?;
        if data.len() as u64 != expected.size {
            return Err(Error::Integrity);
        }
        expected.sha256.verify(&data)?;
        files.insert(path, data);
    }
    if files.len() != manifest.configs.len() {
        return Err(Error::Integrity);
    }
    Ok(files)
}
