//! Portable paths, exact manifests, and resource budgets.
use crate::{Error, Result, model::*};
use std::collections::BTreeSet;

/// Resource limits applied before allocation/decompression where possible.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum JSON response or source file, bytes.
    pub json_bytes: u64,
    /// Maximum downloaded artifact, bytes.
    pub compressed_bytes: u64,
    /// Maximum decoded artifact, bytes.
    pub decoded_bytes: u64,
    /// Maximum individual config, bytes.
    pub file_bytes: u64,
    /// Maximum mods, config entries, requests or API pages (per collection).
    pub entries: usize,
    /// Zstd window log2 maximum, defaults to 27 (128 MiB).
    pub window_log: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            json_bytes: 8 << 20,
            compressed_bytes: 64 << 20,
            decoded_bytes: 256 << 20,
            file_bytes: 16 << 20,
            entries: 10_000,
            window_log: 27,
        }
    }
}
/// Reject unsupported wire versions.
pub fn schema(version: u32) -> Result<()> {
    if version == 1 {
        Ok(())
    } else {
        Err(Error::Schema(version))
    }
}
/// Safe ASCII key for branches, entry identities and publication tags.
pub fn key(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 120
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Invalid("invalid stable identifier".into()));
    }
    Ok(())
}
/// Validate a lowercase or uppercase 40-digit Git commit SHA.
pub fn commit(value: &str) -> Result<()> {
    if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid("expected 40-digit commit SHA".into()));
    }
    Ok(())
}
/// Portable relative path; forbids aliases, traversal, Windows devices and links.
pub fn path(value: &str) -> Result<()> {
    if value.len() > 240
        || value.is_empty()
        || value.contains('\\')
        || value
            .chars()
            .any(|c| c.is_control() || ":*?\"<>|".contains(c))
    {
        return Err(Error::Invalid("unsafe relative path".into()));
    }
    for part in value.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || !part.is_ascii()
        {
            return Err(Error::Invalid("nonportable path component".into()));
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(Error::Invalid("reserved Windows path".into()));
        }
    }
    Ok(())
}
/// Read bounded, strict JSON. Model deserialization rejects unknown fields.
pub fn json<T: serde::de::DeserializeOwned>(bytes: &[u8], limits: &Limits) -> Result<T> {
    if bytes.len() as u64 > limits.json_bytes {
        return Err(Error::Limit("JSON bytes"));
    }
    Ok(serde_json::from_slice(bytes)?)
}
/// Validate manifest structure. Does not prove runtime compatibility of mods.
pub fn manifest(value: &Manifest, limits: &Limits) -> Result<()> {
    schema(value.pack.schema_version)?;
    key(&value.pack.id)?;
    if value.pack.game != "minecraft"
        || !["fabric", "forge", "neoforge", "quilt"].contains(&value.pack.loader.as_str())
        || value.pack.name.is_empty()
        || value.pack.game_version.is_empty()
        || value.pack.loader_version.is_empty()
    {
        return Err(Error::Invalid(
            "pack game, loader, name or exact version missing/unsupported".into(),
        ));
    }
    if value.mods.len().saturating_add(value.configs.len()) > limits.entries {
        return Err(Error::Limit("manifest entries"));
    }
    let mut paths = BTreeSet::new();
    for (id, entry) in &value.mods {
        key(id)?;
        file(&entry.file)?;
        if entry.file.size == 0 {
            return Err(Error::Invalid("empty mod binary".into()));
        }
        if !entry.file.path.starts_with("mods/")
            || entry.name.is_empty()
            || !paths.insert(entry.file.path.to_ascii_lowercase())
        {
            return Err(Error::Invalid(
                "duplicate mod path or missing mod title".into(),
            ));
        }
        match &entry.source {
            Source::Modrinth { project, version } => {
                key(project)?;
                key(version)?;
            }
            Source::Curseforge { project, file } => {
                if [project, file]
                    .iter()
                    .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
                {
                    return Err(Error::Invalid("invalid CurseForge IDs".into()));
                }
            }
            Source::Direct { url } => {
                let u = url::Url::parse(url)
                    .map_err(|_| Error::Invalid("invalid download URL".into()))?;
                if u.scheme() != "https"
                    || u.host_str().is_none()
                    || !u.username().is_empty()
                    || u.password().is_some()
                    || u.query().is_some()
                    || u.fragment().is_some()
                {
                    return Err(Error::Invalid(
                        "download URL must be durable HTTPS without credentials/query/fragment"
                            .into(),
                    ));
                }
            }
        }
        if entry.client == Requirement::Unsupported && entry.server == Requirement::Unsupported {
            return Err(Error::Invalid("mod unsupported on both sides".into()));
        }
    }
    for (p, f) in &value.configs {
        file(f)?;
        if p != &f.path
            || !p.starts_with("config/")
            || f.size > limits.file_bytes
            || !paths.insert(p.to_ascii_lowercase())
        {
            return Err(Error::Invalid(
                "invalid config inventory or path collision".into(),
            ));
        }
    }
    // A path may not simultaneously be a regular file and an ancestor directory.
    for p in &paths {
        for (i, _) in p.match_indices('/') {
            if paths.contains(&p[..i]) {
                return Err(Error::Invalid("file/directory path collision".into()));
            }
        }
    }
    Ok(())
}
fn file(f: &FileSpec) -> Result<()> {
    path(&f.path)
}
/// Validate an index before downloading assets.
pub fn publication(p: &Publication, repository_id: u64, limits: &Limits) -> Result<()> {
    schema(p.schema_version)?;
    key(&p.id)?;
    key(&p.pack_id)?;
    commit(&p.source_commit)?;
    if p.repository_id != repository_id {
        return Err(Error::Integrity);
    }
    asset(&p.manifest, limits)?;
    if let Some(a) = &p.configs {
        asset(a, limits)?;
    }
    Ok(())
}
/// Validate asset sizes and identity. Asset names never become local filenames.
pub fn asset(a: &Asset, limits: &Limits) -> Result<()> {
    if a.id == 0 || a.size > limits.compressed_bytes || a.decoded_size > limits.decoded_bytes {
        return Err(Error::Limit("asset metadata"));
    }
    Ok(())
}
/// Validate request ledger and prevent request-ID aliasing.
pub fn requests(r: &Requests, limits: &Limits) -> Result<()> {
    schema(r.schema_version)?;
    if r.requests.len() > limits.entries || r.applied.len() > limits.entries {
        return Err(Error::Limit("request entries"));
    }
    for (id, req) in &r.requests {
        key(id)?;
        commit(&req.base_commit)?;
        if id != &req.id || req.reason.len() > 8192 {
            return Err(Error::Invalid("invalid request record".into()));
        }
    }
    if r.applied.keys().any(|id| !r.requests.contains_key(id)) {
        return Err(Error::Invalid("applied request missing proposal".into()));
    }
    Ok(())
}
