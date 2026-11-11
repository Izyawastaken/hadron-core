//! Registry-free short repository identifiers. These never grant repository access.
use crate::{Error, Result};

/// Encode a repository ID using Neutrino's checksummed eight-symbol format.
pub fn encode(repository_id: u64) -> Result<String> {
    neutrino_obscura::Code::encode(repository_id)
        .map(|v| v.to_string())
        .map_err(|_| Error::Invalid("repository ID outside Neutrino range".into()))
}
/// Decode a code locally without network access. GitHub authorization is separate.
pub fn decode(code: &str) -> Result<u64> {
    code.parse::<neutrino_obscura::Code>()
        .map(|v| v.repository_id())
        .map_err(|_| Error::Invalid("invalid repository share code".into()))
}
