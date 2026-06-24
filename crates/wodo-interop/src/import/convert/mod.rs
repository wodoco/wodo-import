//! Converters from external issue-trackers into Wodo's `SpaceExport` v2.
//!
//! Each submodule is a PURE function from a tool-specific, already-flattened
//! model into a [`crate::export::types::SpaceExport`] that the existing import
//! pipeline ([`crate::import::build_space_from_export`]) consumes. The
//! converters do no I/O: rich-text inline images are turned into
//! `ExportAttachment` metadata plus an `attachment id → original URL` map that
//! a separate fetcher resolves later.

pub mod adf;
pub mod jira;
pub mod linear;
