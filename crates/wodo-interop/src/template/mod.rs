//! Template types + instantiation helpers (the portable subset).
//!
//! `instantiate_template` and its date/UUID-remapping helpers are pure
//! (Yjs in, Yjs out — no I/O), so they live here and are reused by the import
//! builder. The template *serializer* is not part of this crate (it reads live
//! collaboration docs).

pub mod instantiator;
pub mod types;

pub use instantiator::{instantiate_template, DocumentManifestEntry, InstantiationResult};
pub use types::{TemplateAttachment, TemplateContent};
