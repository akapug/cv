//! The shared token estimator ([`cv_core::ir::TokenKind`]) against the API's own counts.
//!
//! Each row is one request Claude Code 2.1.299 built to resume a copied session (unpruned, after
//! `cv prune`, after `cv prune --drop-thinking`), reduced to byte counts per content kind plus the
//! token count `count_tokens` returned for its messages (system prompt and tools subtracted).
//! Counts only: no transcript content. Measured 2026-10-14.
use cv_core::ir::{est_tokens_bytes, TokenKind, BLOCK_TOKENS, IMAGE_TOKENS};

struct Row {
    label: &'static str,
    model: &'static str,
    content_bytes: [usize; 5],
    images: u64,
    blocks: u64,
    counted_tokens: u64,
}

const KINDS: [TokenKind; 5] = [
    TokenKind::Text,
    TokenKind::ToolInput,
    TokenKind::ToolResult,
    TokenKind::Signature,
    TokenKind::Marker,
];

#[rustfmt::skip]
const MEASURED: &[Row] = &[
    Row { label: "047b2116-default", model: "claude-fable-5", content_bytes: [30978, 39910, 26211, 82880, 9057], images: 1, blocks: 369, counted_tokens: 75846 },
    Row { label: "047b2116-drop", model: "claude-fable-5", content_bytes: [30978, 39910, 26211, 3696, 9057], images: 1, blocks: 287, counted_tokens: 61142 },
    Row { label: "047b2116-orig", model: "claude-fable-5", content_bytes: [30978, 39910, 152758, 82880, 0], images: 17, blocks: 369, counted_tokens: 130475 },
    Row { label: "06f061bb-default", model: "claude-sonnet-5", content_bytes: [151290, 160808, 109098, 2048, 4518], images: 1, blocks: 550, counted_tokens: 191709 },
    Row { label: "06f061bb-drop", model: "claude-sonnet-5", content_bytes: [151290, 160808, 109098, 2048, 4518], images: 1, blocks: 550, counted_tokens: 191689 },
    Row { label: "06f061bb-orig", model: "claude-sonnet-5", content_bytes: [151290, 160808, 136844, 2048, 1106], images: 1, blocks: 550, counted_tokens: 200132 },
    Row { label: "2d1d8414-default", model: "claude-opus-5-5", content_bytes: [67595, 69225, 33644, 119924, 7607], images: 0, blocks: 338, counted_tokens: 80784 },
    Row { label: "2d1d8414-drop", model: "claude-opus-5-5", content_bytes: [63590, 69225, 33644, 6464, 7607], images: 0, blocks: 274, counted_tokens: 80829 },
    Row { label: "2d1d8414-orig", model: "claude-opus-5-5", content_bytes: [67595, 69225, 138679, 119924, 0], images: 0, blocks: 338, counted_tokens: 121124 },
    Row { label: "2f239b62-default", model: "claude-opus-5", content_bytes: [684052, 317777, 47398, 122124, 1090], images: 1, blocks: 429, counted_tokens: 468084 },
    Row { label: "2f239b62-drop", model: "claude-opus-5", content_bytes: [684052, 317777, 47398, 9400, 1090], images: 1, blocks: 408, counted_tokens: 437220 },
    Row { label: "2f239b62-orig", model: "claude-opus-5", content_bytes: [684052, 317777, 47398, 122124, 1090], images: 1, blocks: 429, counted_tokens: 468084 },
    Row { label: "35ef29f6-default", model: "claude-opus-4-6", content_bytes: [75773, 44228, 44353, 25280, 6399], images: 0, blocks: 294, counted_tokens: 71078 },
    Row { label: "35ef29f6-drop", model: "claude-opus-4-6", content_bytes: [75773, 44228, 44353, 0, 6399], images: 0, blocks: 285, counted_tokens: 66031 },
    Row { label: "35ef29f6-orig", model: "claude-opus-4-6", content_bytes: [75773, 44228, 206354, 25280, 0], images: 0, blocks: 294, counted_tokens: 125642 },
    Row { label: "406247d8-default", model: "claude-opus-5", content_bytes: [342714, 751796, 337403, 711324, 30582], images: 0, blocks: 2833, counted_tokens: 821118 },
    Row { label: "406247d8-drop", model: "claude-opus-5", content_bytes: [342714, 751796, 337403, 5276, 30582], images: 0, blocks: 2475, counted_tokens: 663291 },
    Row { label: "406247d8-orig", model: "claude-opus-5", content_bytes: [342714, 751796, 552930, 711324, 10789], images: 6, blocks: 2833, counted_tokens: 907101 },
    Row { label: "418232ec-default", model: "claude-fable-5-1", content_bytes: [772852, 475031, 111831, 705760, 3663], images: 1, blocks: 1438, counted_tokens: 568954 },
    Row { label: "418232ec-drop", model: "claude-fable-5-1", content_bytes: [759032, 475031, 111831, 0, 3663], images: 1, blocks: 1202, counted_tokens: 569005 },
    Row { label: "418232ec-orig", model: "claude-fable-5-1", content_bytes: [772852, 475031, 111831, 705760, 3663], images: 1, blocks: 1438, counted_tokens: 568954 },
    Row { label: "4d416d46-default", model: "claude-opus-4-8", content_bytes: [562197, 490195, 118011, 89416, 18364], images: 5, blocks: 1137, counted_tokens: 545987 },
    Row { label: "4d416d46-drop", model: "claude-opus-4-8", content_bytes: [562197, 490195, 118011, 48896, 18364], images: 5, blocks: 1129, counted_tokens: 534942 },
    Row { label: "4d416d46-orig", model: "claude-opus-4-8", content_bytes: [562197, 490195, 341205, 89416, 0], images: 20, blocks: 1137, counted_tokens: 663733 },
    Row { label: "5a2c83c9-default", model: "claude-opus-4-7", content_bytes: [43046, 9411, 12670, 116100, 2934], images: 3, blocks: 126, counted_tokens: 70342 },
    Row { label: "5a2c83c9-drop", model: "claude-opus-4-7", content_bytes: [43046, 9411, 12670, 27756, 2934], images: 3, blocks: 103, counted_tokens: 47018 },
    Row { label: "5a2c83c9-orig", model: "claude-opus-4-7", content_bytes: [43046, 9411, 268155, 116100, 0], images: 3, blocks: 126, counted_tokens: 178910 },
    Row { label: "61398580-default", model: "claude-opus-4-7", content_bytes: [106611, 238036, 236523, 383288, 4576], images: 0, blocks: 801, counted_tokens: 360762 },
    Row { label: "61398580-drop", model: "claude-opus-4-7", content_bytes: [104576, 238036, 236523, 3100, 4576], images: 0, blocks: 643, counted_tokens: 262633 },
    Row { label: "61398580-orig", model: "claude-opus-4-7", content_bytes: [106611, 238036, 375195, 383288, 0], images: 0, blocks: 801, counted_tokens: 414252 },
    Row { label: "6418429c-default", model: "claude-opus-5", content_bytes: [1117480, 662561, 174397, 649148, 5920], images: 0, blocks: 1400, counted_tokens: 959708 },
    Row { label: "6418429c-drop", model: "claude-opus-5", content_bytes: [1117480, 662561, 174397, 3468, 5920], images: 0, blocks: 1175, counted_tokens: 796179 },
    Row { label: "6418429c-orig", model: "claude-opus-5", content_bytes: [1117480, 662561, 176593, 649148, 5571], images: 0, blocks: 1400, counted_tokens: 960601 },
    Row { label: "70642dc2-default", model: "claude-opus-4-6", content_bytes: [890624, 532502, 354425, 61188, 11289], images: 10, blocks: 1186, counted_tokens: 541255 },
    Row { label: "70642dc2-drop", model: "claude-opus-4-6", content_bytes: [859345, 532502, 354425, 0, 11289], images: 10, blocks: 1165, counted_tokens: 530512 },
    Row { label: "70642dc2-orig", model: "claude-opus-4-6", content_bytes: [890624, 532502, 592273, 61188, 0], images: 10, blocks: 1186, counted_tokens: 606969 },
    Row { label: "86f445c3-default", model: "claude-opus-4-8", content_bytes: [852325, 430974, 86980, 1475472, 375], images: 1, blocks: 848, counted_tokens: 971587 },
    Row { label: "86f445c3-drop", model: "claude-opus-4-8", content_bytes: [852325, 430974, 86980, 25868, 375], images: 1, blocks: 667, counted_tokens: 570403 },
    Row { label: "86f445c3-orig", model: "claude-opus-4-8", content_bytes: [852325, 430974, 86980, 1475472, 375], images: 1, blocks: 848, counted_tokens: 971587 },
    Row { label: "c198d7f7-default", model: "claude-fable-5", content_bytes: [718288, 397236, 99870, 456708, 972], images: 2, blocks: 1260, counted_tokens: 611491 },
    Row { label: "c198d7f7-drop", model: "claude-fable-5", content_bytes: [718288, 397236, 99870, 10512, 972], images: 2, blocks: 1059, counted_tokens: 511129 },
    Row { label: "c198d7f7-orig", model: "claude-fable-5", content_bytes: [718288, 397236, 102216, 456708, 647], images: 2, blocks: 1260, counted_tokens: 612141 },
];

fn estimate(row: &Row) -> u64 {
    let content: u64 = row
        .content_bytes
        .iter()
        .zip(KINDS)
        .map(|(b, kind)| est_tokens_bytes(*b, kind))
        .sum();
    content + row.images * IMAGE_TOKENS + row.blocks * BLOCK_TOKENS
}

/// The estimate errs high: never more than 8% under the count, and median error within 10%.
/// The +60% upper bound is a loose sanity ceiling; underestimation has the tighter gate.
/// (The claude-opus-4-6 tokenizer is sparser, so its rows read high; the rows that read highest
/// are sessions whose thinking signatures the API did not count.)
#[test]
fn per_kind_estimate_tracks_measured_counts() {
    let mut errors: Vec<(f64, &Row)> = MEASURED
        .iter()
        .map(|row| {
            (
                (estimate(row) as f64 - row.counted_tokens as f64) / row.counted_tokens as f64,
                row,
            )
        })
        .collect();
    errors.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (low, row) = errors[0];
    assert!(
        low > -0.08,
        "{} ({}) estimated {:.1}% under its count",
        row.label,
        row.model,
        -100.0 * low
    );
    let (high, row) = errors[errors.len() - 1];
    assert!(
        high < 0.6,
        "{} ({}) estimated {:.1}% over its count",
        row.label,
        row.model,
        100.0 * high
    );
    let median = errors[errors.len() / 2].0;
    assert!(median.abs() < 0.1, "median error {:.1}%", 100.0 * median);
}

/// `--drop-thinking` omits blocks outright, so the request it resumes is never larger than the
/// default prune's (the two sessions whose signatures the API ignores differ by under 0.1%).
#[test]
fn drop_thinking_never_grows_the_measured_request() {
    for row in MEASURED.iter().filter(|row| row.label.ends_with("-drop")) {
        let session = row.label.trim_end_matches("-drop");
        let default = MEASURED
            .iter()
            .find(|other| other.label == format!("{session}-default"))
            .expect("every session was measured both ways");
        assert!(
            row.counted_tokens as f64 <= default.counted_tokens as f64 * 1.001,
            "{session}: drop-thinking {} > default {}",
            row.counted_tokens,
            default.counted_tokens
        );
        assert!(
            row.content_bytes[4] <= default.content_bytes[4],
            "{session}: drop-thinking added marker bytes"
        );
    }
}
