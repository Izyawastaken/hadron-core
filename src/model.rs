//! Version 1 wire models. Unknown fields are rejected to prevent lossy edits.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Current wire schema; independent of the crate's semver.
pub const SCHEMA_VERSION: u32 = 1;

/// SHA-256 in lowercase hex. Constructors and deserialization verify its shape.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Hash(String);
impl Hash {
    /// Hash bytes without copying them.
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }
    /// Parse canonical lowercase SHA-256 hex.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid("expected lowercase SHA-256".into()));
        }
        Ok(Self(value))
    }
    /// Canonical hexadecimal digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Verify content against this digest.
    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        if *self == Self::of(bytes) {
            Ok(())
        } else {
            Err(Error::Integrity)
        }
    }
}
impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// A file expected in an instance, with strong integrity metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    /// Portable relative destination, e.g. `mods/sodium.jar`.
    pub path: String,
    /// Exact byte count.
    pub size: u64,
    /// SHA-256 of file bytes. Obtain it before publication, even for SHA-1-only providers.
    pub sha256: Hash,
}

/// Download provenance; provider IDs are strings to avoid frontend integer loss.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// A pinned Modrinth file.
    Modrinth {
        /// Stable project ID.
        project: String,
        /// Exact version ID.
        version: String,
    },
    /// A pinned CurseForge file, resolved by Tachyon using permitted API access.
    Curseforge {
        /// Stable project ID.
        project: String,
        /// Exact file ID.
        file: String,
    },
    /// A durable HTTPS file URL without embedded credentials or query parameters.
    Direct {
        /// Original author-approved URL.
        url: String,
    },
}

/// Whether a mod is applicable on a client or dedicated server.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// Must install on this side.
    Required,
    /// User may elect to install on this side.
    Optional,
    /// Must not install on this side.
    Unsupported,
}

/// Exact, provider-resolved mod specification; no floating `latest` versions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModEntry {
    /// Human-facing title.
    pub name: String,
    /// File, hash and target path.
    pub file: FileSpec,
    /// Pinned provider or direct source.
    pub source: Source,
    /// Client applicability.
    pub client: Requirement,
    /// Dedicated server applicability.
    pub server: Requirement,
}

/// Editable `pack.json` data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    /// Wire schema, currently 1.
    pub schema_version: u32,
    /// Stable pack identity (not its display name).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Human description.
    pub description: String,
    /// Game identifier; 0.0.1 supports `minecraft`.
    pub game: String,
    /// Exact game version.
    pub game_version: String,
    /// Loader, e.g. `fabric`, `forge`, `neoforge`, `quilt`.
    pub loader: String,
    /// Exact loader version.
    pub loader_version: String,
}

/// Editable `mods.json`; sorted keys make changes deterministic.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModList {
    /// Stable entry key to exact file metadata.
    pub mods: BTreeMap<String, ModEntry>,
}

/// Published manifest, combining authoritative pack, mods and config inventory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Pack metadata including schema version.
    pub pack: Pack,
    /// Exact mod entries.
    pub mods: BTreeMap<String, ModEntry>,
    /// Config path to exact file metadata. Only `config/` destinations are allowed.
    pub configs: BTreeMap<String, FileSpec>,
}

/// A SHA-pinned operation. Expected entries provide optimistic concurrency checks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    /// Add a previously absent entry.
    Add {
        /// Stable mod key.
        key: String,
        /// Complete exact metadata.
        entry: ModEntry,
    },
    /// Remove exactly the reviewed entry.
    Remove {
        /// Stable mod key.
        key: String,
        /// Entry expected to be installed before this operation.
        expected: ModEntry,
    },
    /// Replace exactly the reviewed entry.
    Update {
        /// Stable mod key.
        key: String,
        /// Expected old metadata.
        expected: ModEntry,
        /// Complete replacement metadata.
        entry: ModEntry,
    },
}
impl Operation {
    /// Apply to a copy and return it, failing without partial mutation on conflict.
    pub fn apply(&self, list: &ModList) -> Result<ModList> {
        let mut next = list.clone();
        match self {
            Self::Add { key, entry } => {
                if next.mods.contains_key(key) {
                    return Err(Error::Conflict("mod already present".into()));
                }
                next.mods.insert(key.clone(), entry.clone());
            }
            Self::Remove { key, expected } | Self::Update { key, expected, .. } => {
                if next.mods.get(key) != Some(expected) {
                    return Err(Error::Conflict("mod changed since request".into()));
                }
                match self {
                    Self::Update { entry, .. } => {
                        next.mods.insert(key.clone(), entry.clone());
                    }
                    _ => {
                        next.mods.remove(key);
                    }
                }
            }
        }
        Ok(next)
    }
}

/// A user's proposal; JSON fields confer no GitHub permissions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Unique retry/reconciliation key; use a UUID or similarly unique safe identifier.
    pub id: String,
    /// Exact commit the requester inspected.
    pub base_commit: String,
    /// Current publication when proposed, if any.
    pub base_publication: Option<String>,
    /// User's explanation.
    pub reason: String,
    /// Reviewed concrete change.
    pub operation: Operation,
}

/// Request ledger. Membership in `applied` is written atomically with `mods.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requests {
    /// Wire schema, currently 1.
    pub schema_version: u32,
    /// Requests keyed by ID, including merged proposals not yet applied.
    pub requests: BTreeMap<String, Request>,
    /// Applied request ID to GitHub PR number. The containing commit identifies application.
    pub applied: BTreeMap<String, u64>,
}
impl Default for Requests {
    fn default() -> Self {
        Self {
            schema_version: 1,
            requests: BTreeMap::new(),
            applied: BTreeMap::new(),
        }
    }
}

/// Immutable release asset identity. API routes are derived locally, never from URLs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    /// Numeric GitHub release asset ID.
    pub id: u64,
    /// Uploaded name; descriptive only, never used as a local path.
    pub name: String,
    /// Compressed or raw bytes on GitHub.
    pub size: u64,
    /// Hash of those exact bytes.
    pub sha256: Hash,
    /// Decoded byte count, equal to size for uncompressed publication JSON.
    pub decoded_size: u64,
    /// Hash of decoded content.
    pub decoded_sha256: Hash,
}

/// Immutable publication index, also stored as `publication.json` release asset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    /// Wire schema.
    pub schema_version: u32,
    /// Repository numeric identity.
    pub repository_id: u64,
    /// Stable pack identity.
    pub pack_id: String,
    /// Unique publication identifier, used in tag `hadron-<id>`.
    pub id: String,
    /// Exact source snapshot.
    pub source_commit: String,
    /// Zstandard compressed manifest.
    pub manifest: Asset,
    /// Zstandard compressed deterministic tar, or none for an empty config inventory.
    pub configs: Option<Asset>,
}

/// Mutable `.hadron/latest.json` pointer, advanced only after complete upload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Latest {
    /// Wire schema.
    pub schema_version: u32,
    /// Publication identifier.
    pub publication_id: String,
    /// Raw `publication.json` asset and its integrity metadata.
    pub index: Asset,
}
