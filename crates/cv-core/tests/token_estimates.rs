//! The shared token estimator ([`cv_core::ir::TokenKind`]) against the API's own counts.
//!
//! Each row is one request Claude Code 2.1.299 built to resume a copied session (unpruned, after
//! `cv prune`, after `cv prune --drop-thinking`), reduced to byte counts per content kind plus the
//! token count `count_tokens` returned for its messages (system prompt and tools subtracted).
//! Counts only: no transcript content. Measured 2026-10-14.
use cv_core::ir::{est_tokens_bytes, TokenKind, BLOCK_TOKENS, IMAGE_TOKENS};

type Row = (&'static str, &'static str, [usize; 5], u64, u64, u64);

const KINDS: [TokenKind; 5] = [
    TokenKind::Text,
    TokenKind::ToolInput,
    TokenKind::ToolResult,
    TokenKind::Signature,
    TokenKind::Marker,
];

#[rustfmt::skip]
const MEASURED: &[Row] = &[
    ("047b2116-default", "claude-fable-5", [30978, 39910, 26211, 82880, 9057], 1, 369, 75846),
    ("047b2116-drop", "claude-fable-5", [30978, 39910, 26211, 3696, 9057], 1, 287, 61142),
    ("047b2116-orig", "claude-fable-5", [30978, 39910, 152758, 82880, 0], 17, 369, 130475),
    ("06f061bb-default", "claude-sonnet-5", [151290, 160808, 109098, 2048, 4518], 1, 550, 191709),
    ("06f061bb-drop", "claude-sonnet-5", [151290, 160808, 109098, 2048, 4518], 1, 550, 191689),
    ("06f061bb-orig", "claude-sonnet-5", [151290, 160808, 136844, 2048, 1106], 1, 550, 200132),
    ("2d1d8414-default", "claude-opus-5-5", [67595, 69225, 33644, 119924, 7607], 0, 338, 80784),
    ("2d1d8414-drop", "claude-opus-5-5", [63590, 69225, 33644, 6464, 7607], 0, 274, 80829),
    ("2d1d8414-orig", "claude-opus-5-5", [67595, 69225, 138679, 119924, 0], 0, 338, 121124),
    ("2f239b62-default", "claude-opus-5", [684052, 317777, 47398, 122124, 1090], 1, 429, 468084),
    ("2f239b62-drop", "claude-opus-5", [684052, 317777, 47398, 9400, 1090], 1, 408, 437220),
    ("2f239b62-orig", "claude-opus-5", [684052, 317777, 47398, 122124, 1090], 1, 429, 468084),
    ("35ef29f6-default", "claude-opus-4-6", [75773, 44228, 44353, 25280, 6399], 0, 294, 71078),
    ("35ef29f6-drop", "claude-opus-4-6", [75773, 44228, 44353, 0, 6399], 0, 285, 66031),
    ("35ef29f6-orig", "claude-opus-4-6", [75773, 44228, 206354, 25280, 0], 0, 294, 125642),
    ("406247d8-default", "claude-opus-5", [342714, 751796, 337403, 711324, 30582], 0, 2833, 821118),
    ("406247d8-drop", "claude-opus-5", [342714, 751796, 337403, 5276, 30582], 0, 2475, 663291),
    ("406247d8-orig", "claude-opus-5", [342714, 751796, 552930, 711324, 10789], 6, 2833, 907101),
    ("418232ec-default", "claude-fable-5-1", [772852, 475031, 111831, 705760, 3663], 1, 1438, 568954),
    ("418232ec-drop", "claude-fable-5-1", [759032, 475031, 111831, 0, 3663], 1, 1202, 569005),
    ("418232ec-orig", "claude-fable-5-1", [772852, 475031, 111831, 705760, 3663], 1, 1438, 568954),
    ("4d416d46-default", "claude-opus-4-8", [562197, 490195, 118011, 89416, 18364], 5, 1137, 545987),
    ("4d416d46-drop", "claude-opus-4-8", [562197, 490195, 118011, 48896, 18364], 5, 1129, 534942),
    ("4d416d46-orig", "claude-opus-4-8", [562197, 490195, 341205, 89416, 0], 20, 1137, 663733),
    ("5a2c83c9-default", "claude-opus-4-7", [43046, 9411, 12670, 116100, 2934], 3, 126, 70342),
    ("5a2c83c9-drop", "claude-opus-4-7", [43046, 9411, 12670, 27756, 2934], 3, 103, 47018),
    ("5a2c83c9-orig", "claude-opus-4-7", [43046, 9411, 268155, 116100, 0], 3, 126, 178910),
    ("61398580-default", "claude-opus-4-7", [106611, 238036, 236523, 383288, 4576], 0, 801, 360762),
    ("61398580-drop", "claude-opus-4-7", [104576, 238036, 236523, 3100, 4576], 0, 643, 262633),
    ("61398580-orig", "claude-opus-4-7", [106611, 238036, 375195, 383288, 0], 0, 801, 414252),
    ("6418429c-default", "claude-opus-5", [1117480, 662561, 174397, 649148, 5920], 0, 1400, 959708),
    ("6418429c-drop", "claude-opus-5", [1117480, 662561, 174397, 3468, 5920], 0, 1175, 796179),
    ("6418429c-orig", "claude-opus-5", [1117480, 662561, 176593, 649148, 5571], 0, 1400, 960601),
    ("70642dc2-default", "claude-opus-4-6", [890624, 532502, 354425, 61188, 11289], 10, 1186, 541255),
    ("70642dc2-drop", "claude-opus-4-6", [859345, 532502, 354425, 0, 11289], 10, 1165, 530512),
    ("70642dc2-orig", "claude-opus-4-6", [890624, 532502, 592273, 61188, 0], 10, 1186, 606969),
    ("86f445c3-default", "claude-opus-4-8", [852325, 430974, 86980, 1475472, 375], 1, 848, 971587),
    ("86f445c3-drop", "claude-opus-4-8", [852325, 430974, 86980, 25868, 375], 1, 667, 570403),
    ("86f445c3-orig", "claude-opus-4-8", [852325, 430974, 86980, 1475472, 375], 1, 848, 971587),
    ("c198d7f7-default", "claude-fable-5", [718288, 397236, 99870, 456708, 972], 2, 1260, 611491),
    ("c198d7f7-drop", "claude-fable-5", [718288, 397236, 99870, 10512, 972], 2, 1059, 511129),
    ("c198d7f7-orig", "claude-fable-5", [718288, 397236, 102216, 456708, 647], 2, 1260, 612141),
];

fn estimate(row: &Row) -> u64 {
    let (_, _, bytes, images, blocks, _) = row;
    let content: u64 = bytes
        .iter()
        .zip(KINDS)
        .map(|(b, kind)| est_tokens_bytes(*b, kind))
        .sum();
    content + images * IMAGE_TOKENS + blocks * BLOCK_TOKENS
}

/// The estimate errs high: never more than 8% under the count, and the middle row within 10%.
/// (The claude-opus-4-6 tokenizer is sparser, so its rows read high; the rows that read highest
/// are sessions whose thinking signatures the API did not count.)
#[test]
fn per_kind_estimate_tracks_measured_counts() {
    let mut errors: Vec<(f64, &str)> = MEASURED
        .iter()
        .map(|row| ((estimate(row) as f64 - row.5 as f64) / row.5 as f64, row.0))
        .collect();
    errors.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (low, label) = errors[0];
    assert!(low > -0.08, "{label} estimated {:.1}% under its count", -100.0 * low);
    let (high, label) = errors[errors.len() - 1];
    assert!(high < 0.6, "{label} estimated {:.1}% over its count", 100.0 * high);
    let median = errors[errors.len() / 2].0;
    assert!(median.abs() < 0.1, "median error {:.1}%", 100.0 * median);
}

/// `--drop-thinking` omits blocks outright, so the request it resumes is never larger than the
/// default prune's (the two sessions whose signatures the API ignores differ by under 0.1%).
#[test]
fn drop_thinking_never_grows_the_measured_request() {
    for row in MEASURED.iter().filter(|row| row.0.ends_with("-drop")) {
        let session = row.0.trim_end_matches("-drop");
        let default = MEASURED
            .iter()
            .find(|other| other.0 == format!("{session}-default"))
            .expect("every session was measured both ways");
        assert!(
            row.5 as f64 <= default.5 as f64 * 1.001,
            "{session}: drop-thinking {} > default {}",
            row.5,
            default.5
        );
        assert!(row.2[4] <= default.2[4], "{session}: drop-thinking added marker bytes");
    }
}
