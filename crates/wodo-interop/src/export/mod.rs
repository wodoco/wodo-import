//! Space export: the portable `SpaceExport` types and ZIP packaging.
//!
//! The serializer (live Yjs doc → `SpaceExport`) and the anonymizer are not
//! part of this crate — they depend on server-side search/extraction. Only the
//! type definitions and the archive writer are portable, and they live here.

pub mod package;
pub mod types;
