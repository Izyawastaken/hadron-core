#![forbid(unsafe_code)]
#![deny(missing_docs, rustdoc::broken_intra_doc_links)]
#![doc = include_str!("../docs/guide.md")]

#[cfg(feature = "cache")]
pub mod cache;
#[cfg(feature = "compression")]
pub mod codec;
pub mod diff;
pub mod error;
#[cfg(feature = "github")]
pub mod github;
pub mod model;
pub mod share;
pub mod validation;
#[cfg(feature = "github")]
pub mod workflow;
pub use error::{Error, Result};
pub use model::*;
