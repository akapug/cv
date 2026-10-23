//! cv-web — WASM bindings for clustervision.
//!
//! The browser app uploads a harness directory as a `.zip`; we unzip it entirely in memory (no
//! filesystem), sniff + parse each entry via [`cv_core::ingest`], and hand back the sessions as a
//! JSON string for the frontend.

use std::io::Cursor;
use wasm_bindgen::prelude::*;

/// Ingest a complete bounded harness archive. Extraction rejects corrupt or oversized uploads
/// outright, but transcripts whose harness cannot be sniffed are skipped silently: they produce
/// no session and are not reported back, so a partly-recognised archive returns fewer sessions
/// than it held files.
#[wasm_bindgen]
pub fn ingest_zip(bytes: &[u8]) -> Result<String, JsValue> {
    let files = cv_core::ingest::archive::extract(Cursor::new(bytes), Default::default())
        .map_err(|error| JsValue::from_str(&format!("zip: {error:#}")))?;
    let sessions =
        cv_core::ingest::ingest_files(files).map_err(|error| JsValue::from_str(&format!("transcript: {error:#}")))?;
    serde_json::to_string(&sessions).map_err(|error| JsValue::from_str(&format!("json: {error}")))
}
