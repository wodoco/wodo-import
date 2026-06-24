//! Wodo interop: the pure import/export core for Wodo's data portability.
//!
//! Everything needed to convert an external issue tracker (Jira, Linear, …)
//! into a `wodo-space-export-v2` archive, and to rebuild a space's Yjs
//! documents from one. It depends on no server infrastructure — no database,
//! no search index, no object storage, no realtime-collab server — so it is
//! safe to reuse anywhere a `SpaceExport` is produced or consumed.
//!
//! - [`import`] — `build_space_from_export` + the Jira/Linear converters & fetchers
//! - [`export`] — the `SpaceExport` types and ZIP packaging
//! - [`markdown`] — Markdown → Yjs `XmlFragment` encoding
//! - [`template`] — template type definitions + instantiation helpers
//! - [`filter_token`] — saved-view filter UUID codec

pub mod export;
pub mod filter_token;
pub mod import;
pub mod markdown;
pub mod template;
pub mod yrs_xml_copy;
