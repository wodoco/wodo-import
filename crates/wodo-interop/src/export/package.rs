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

use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::export::types::SpaceExport;

/// Write a SpaceExport plus its attachment blobs into a deflate ZIP archive.
///
/// `attachments` is `(att_id, filename, bytes)`; each becomes the entry
/// `attachments/{att_id}/{filename}`. `data.json` holds the serialized export.
/// Returns the in-memory ZIP bytes.
pub fn write_space_export_zip(
    export: &SpaceExport,
    attachments: &[(String, String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec_pretty(export).map_err(|e| format!("serialize export: {e}"))?;

    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("data.json", options)
        .map_err(|e| format!("start data.json: {e}"))?;
    zip.write_all(&json)
        .map_err(|e| format!("write data.json: {e}"))?;

    for (att_id, filename, bytes) in attachments {
        let entry = format!("attachments/{att_id}/{filename}");
        zip.start_file(&entry, options)
            .map_err(|e| format!("start {entry}: {e}"))?;
        zip.write_all(bytes)
            .map_err(|e| format!("write {entry}: {e}"))?;
    }

    let cursor = zip.finish().map_err(|e| format!("finish zip: {e}"))?;
    Ok(cursor.into_inner())
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
