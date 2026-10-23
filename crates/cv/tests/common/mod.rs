//! The two-session fixture corpus shared by the integration tests that need "a normal corpus":
//! `alphasess` (a title + one user/assistant round) and `betasess` (one round), as claude-format
//! transcripts under the world's fake `$HOME`.

use cv_testkit::World;
use serde_json::json;
use std::fs;

/// A world with the two-session fixture corpus every test starts from.
pub fn world(tag: &str) -> World {
    let w = World::new(tag);
    write_fixtures(&w);
    w
}

pub fn write_fixtures(w: &World) {
    let alpha = [
        json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
        json!({"type": "user", "uuid": "u1", "timestamp": "2026-01-01T10:00:00Z",
               "cwd": "/work/proj",
               "message": {"role": "user", "content": "please fix the zebrafish migration"}}),
        json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-01-02T10:00:00Z",
               "message": {"role": "assistant", "content": [
                   {"type": "text", "text": "done"}]}}),
    ];
    let beta = [
        json!({"type": "user", "uuid": "b1", "timestamp": "2026-03-01T09:00:00Z",
               "cwd": "/work/proj",
               "message": {"role": "user", "content": "quokka census"}}),
        json!({"type": "assistant", "uuid": "b2", "timestamp": "2026-03-01T09:05:00Z",
               "message": {"role": "assistant", "content": [
                   {"type": "text", "text": "seventeen"}]}}),
    ];
    for (name, lines) in [("alphasess", &alpha[..]), ("betasess", &beta[..])] {
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(w.transcript(name), body).unwrap();
    }
}
