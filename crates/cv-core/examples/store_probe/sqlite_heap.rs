//! Opt-in process-local SQLite allocator accounting. No allocator replacement or RSS sampling.
use anyhow::{ensure, Result};
use rusqlite::ffi;
use serde::Serialize;

fn status(code: i32, reset: bool) -> Result<(i64, i64)> {
    let (mut current, mut highwater) = (0, 0);
    // SQLite writes exactly these two sqlite3_int64 slots; status identifiers are constants.
    let rc = unsafe { ffi::sqlite3_status64(code, &mut current, &mut highwater, i32::from(reset)) };
    ensure!(rc == ffi::SQLITE_OK, "SQLite status {code} unavailable: {rc}");
    Ok((current, highwater))
}

pub struct Accounting {
    verified_allocation_bytes: i64,
}

pub struct Baseline {
    bytes: i64,
    allocations: i64,
}

#[derive(Serialize)]
pub struct Snapshot {
    accounting_enabled_and_verified: bool,
    verified_allocation_bytes: i64,
    baseline_live_bytes: i64,
    peak_live_bytes: i64,
    peak_above_baseline_bytes: i64,
    stage_end_live_bytes: i64,
    baseline_live_allocations: i64,
    peak_live_allocations: i64,
    stage_end_live_allocations: i64,
    largest_allocation_request_bytes: i64,
    // SQLite's built-in status API reports live allocation counts, not cumulative requests, so
    // the snapshot has no cumulative fields: it reports only what it measured.
}

impl Accounting {
    /// Called before any adapter/catalog/SQLite setup, in this isolated one-operation process.
    pub fn enable() -> Result<Self> {
        // MEMSTATUS is a pre-initialization setting. Never shutdown someone else's connection.
        let configured = unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_MEMSTATUS, 1i32) };
        ensure!(
            configured == ffi::SQLITE_OK,
            "cannot enable SQLite memory accounting before initialization: {configured}"
        );
        let initialized = unsafe { ffi::sqlite3_initialize() };
        ensure!(
            initialized == ffi::SQLITE_OK,
            "SQLite initialization failed: {initialized}"
        );
        let before = unsafe { ffi::sqlite3_memory_used() };
        let count_before = status(ffi::SQLITE_STATUS_MALLOC_COUNT, false)?.0;
        // A successful known allocation proves the counters are operational, even if this build
        // changes MEMSTATUS defaults or omits accounting. Free it before taking the stage baseline.
        let allocation = unsafe { ffi::sqlite3_malloc(4096) };
        ensure!(
            !allocation.is_null(),
            "SQLite accounting verification allocation failed"
        );
        let after = unsafe { ffi::sqlite3_memory_used() };
        let count_after = status(ffi::SQLITE_STATUS_MALLOC_COUNT, false);
        unsafe { ffi::sqlite3_free(allocation) };
        ensure!(
            after - before >= 4096 && count_after?.0 > count_before,
            "SQLite memory accounting disabled or non-operational"
        );
        Ok(Self {
            verified_allocation_bytes: after - before,
        })
    }

    pub fn begin(&self) -> Result<Baseline> {
        let bytes = unsafe { ffi::sqlite3_memory_used() };
        unsafe { ffi::sqlite3_memory_highwater(1) };
        let allocations = status(ffi::SQLITE_STATUS_MALLOC_COUNT, true)?.0;
        // The current slot for MALLOC_SIZE is undefined; use only its reset high-water slot.
        status(ffi::SQLITE_STATUS_MALLOC_SIZE, true)?;
        Ok(Baseline { bytes, allocations })
    }

    pub fn end(&self, baseline: Baseline) -> Result<Snapshot> {
        let end = unsafe { ffi::sqlite3_memory_used() };
        let peak = unsafe { ffi::sqlite3_memory_highwater(0) };
        let (allocations, peak_allocations) = status(ffi::SQLITE_STATUS_MALLOC_COUNT, false)?;
        let largest_request = status(ffi::SQLITE_STATUS_MALLOC_SIZE, false)?.1;
        Ok(Snapshot {
            accounting_enabled_and_verified: true,
            verified_allocation_bytes: self.verified_allocation_bytes,
            baseline_live_bytes: baseline.bytes,
            peak_live_bytes: peak,
            peak_above_baseline_bytes: peak.saturating_sub(baseline.bytes),
            stage_end_live_bytes: end,
            baseline_live_allocations: baseline.allocations,
            peak_live_allocations: peak_allocations,
            stage_end_live_allocations: allocations,
            largest_allocation_request_bytes: largest_request,
        })
    }
}
