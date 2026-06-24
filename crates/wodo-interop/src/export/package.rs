//! Package a [`SpaceExport`] into the import-wizard ZIP archive.
//!
//! The archive layout mirrors what the export pipeline emits and what
//! [`crate::import::build_space_from_export`] (via the import wizard) consumes:
//!
//! ```text
//! data.json                         ← serde_json of the SpaceExport
//! attachments/{att_id}/{filename}   ← one entry per downloaded attachment blob
//! ```
//!
//! This is a write-side mirror of the ZIP reader/writer in
//! `wodo-admin/src/commands/anonymize.rs` (same `ZipWriter` / `SimpleFileOptions`
//! / deflate layout); it exists here so the Linear importer can build a valid
//! archive without depending on the admin crate.

use std::io::{Cursor, Seek, SeekFrom, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::export::types::SpaceExport;

/// Streaming writer for the import-wizard ZIP.
///
/// Attachments are added one at a time and written straight through to the
/// caller-provided sink `W`, so the whole archive never lives in memory — only
/// one blob at a time. `W` is the injected destination: a local file for the
/// CLI, an S3-staging sink on the server. `data.json` is written **last**
/// (in [`finish`](Self::finish)) so the caller can backfill the export — e.g.
/// real attachment sizes learned during download — before it's serialized; ZIP
/// entry order is irrelevant to readers, which use the central directory.
pub struct SpaceExportZip<W: Write + Seek> {
    zip: ZipWriter<W>,
    options: SimpleFileOptions,
}

impl<W: Write + Seek> SpaceExportZip<W> {
    /// Open the archive over `writer`. The sink must be **seekable**: the ZIP
    /// format patches each local header with sizes/CRC after the entry data, so
    /// `ZipWriter` seeks back over it. A local file satisfies this directly; a
    /// non-seekable target (e.g. S3 multipart) is fed via a temp-file-backed
    /// handle.
    pub fn new(writer: W) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            options: SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        }
    }

    /// Stream one attachment blob into `attachments/{att_id}/{filename}`.
    pub fn add_attachment(
        &mut self,
        att_id: &str,
        filename: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        let entry = format!("attachments/{att_id}/{filename}");
        self.zip
            .start_file(&entry, self.options)
            .map_err(|e| format!("start {entry}: {e}"))?;
        self.zip
            .write_all(bytes)
            .map_err(|e| format!("write {entry}: {e}"))?;
        Ok(())
    }

    /// Write `data.json` (the serialized export) and close the archive.
    /// Returns the archive size in bytes.
    pub fn finish(mut self, export: &SpaceExport) -> Result<u64, String> {
        let json =
            serde_json::to_vec_pretty(export).map_err(|e| format!("serialize export: {e}"))?;
        self.zip
            .start_file("data.json", self.options)
            .map_err(|e| format!("start data.json: {e}"))?;
        self.zip
            .write_all(&json)
            .map_err(|e| format!("write data.json: {e}"))?;
        let mut writer = self.zip.finish().map_err(|e| format!("finish zip: {e}"))?;
        writer.flush().map_err(|e| format!("flush zip: {e}"))?;
        // Archive size = final stream length (robust to ZipWriter's header
        // seek-backs, which a byte counter would over-count).
        writer
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("size zip: {e}"))
    }
}

/// Convenience: build the whole archive in memory and return its bytes. Prefer
/// [`SpaceExportZip`] for large or streamed exports; this is for small exports
/// and tests.
pub fn write_space_export_zip(
    export: &SpaceExport,
    attachments: &[(String, String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zw = SpaceExportZip::new(&mut buf);
        for (att_id, filename, bytes) in attachments {
            zw.add_attachment(att_id, filename, bytes)?;
        }
        zw.finish(export)?;
    }
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{build_space_from_export, ImportMaps};
    use std::io::Read;
    use yrs::{ReadTxn, Transact};

    const ATTACH: &str = "77777777-7777-7777-7777-777777777777";

    fn tiny_export() -> SpaceExport {
        serde_json::from_value(serde_json::json!({
            "format": "wodo-space-export-v2",
            "exported_at": "2026-06-11T00:00:00Z",
            "space": {
                "id": "66666666-6666-6666-6666-666666666666",
                "name": "Demo", "slug": "demo", "region": "eu",
                "short_id_visible": true, "created_at": "2026-01-01T00:00:00Z"
            },
            "labels": {"order": [], "primary_label_id": null, "definitions": {}},
            "milestones": {"order": [], "definitions": {}},
            "cycles": [],
            "views": {"order": [], "definitions": {}},
            "items": [],
            "documents": [],
            "attachments": [{
                "id": ATTACH, "filename": "report.pdf",
                "content_type": "application/pdf", "size_bytes": 3,
                "uploaded_by": "22222222-2222-2222-2222-222222222222",
                "uploaded_at": "2026-05-02T09:14:00Z",
                "orphaned": false
            }],
            "users": [],
            "teams": []
        }))
        .unwrap()
    }

    #[test]
    fn round_trips_and_imports() {
        let export = tiny_export();
        let blobs = vec![(
            ATTACH.to_string(),
            "report.pdf".to_string(),
            b"PDF".to_vec(),
        )];

        let zip_bytes = write_space_export_zip(&export, &blobs).unwrap();
        assert!(zip_bytes.starts_with(b"PK"), "output is a ZIP");

        let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"data.json".to_string()));
        assert!(
            names.contains(&format!("attachments/{ATTACH}/report.pdf")),
            "attachment entry present: {names:?}"
        );

        // data.json parses back into a SpaceExport …
        let mut json = Vec::new();
        archive
            .by_name("data.json")
            .unwrap()
            .read_to_end(&mut json)
            .unwrap();
        let parsed: SpaceExport = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.space.name, "Demo");

        // … and is import-valid (build_space_from_export must not panic).
        let result = build_space_from_export(
            &parsed,
            "bbbbbbbb-0000-0000-0000-000000000002",
            "cccccccc-0000-0000-0000-000000000003",
            &ImportMaps::default(),
        );
        assert!(result.space_doc.transact().get_map("config").is_some());

        // The attachment blob survives the round-trip byte-for-byte.
        let mut pdf = Vec::new();
        archive
            .by_name(&format!("attachments/{ATTACH}/report.pdf"))
            .unwrap()
            .read_to_end(&mut pdf)
            .unwrap();
        assert_eq!(pdf, b"PDF");
    }
}
