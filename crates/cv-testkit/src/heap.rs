//! Requested live Rust allocation counters for isolated diagnostic processes.
//! Installing `Allocator` is opt-in; the GlobalAlloc implementation below defines what is counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub struct Allocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);

fn added(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
    CALLS.fetch_add(1, Relaxed);
    REQUESTED.fetch_add(bytes, Relaxed);
}

// No allocation in these hooks. Count successful Rust requests, including realloc growth;
// SQLite/C allocations, mmap, stacks and allocator overhead are deliberately outside this.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            added(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            added(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(p, layout, size) };
        if !out.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            added(size);
        }
        out
    }
}

/// An allocation observation started by [`begin`], consumed exactly once by [`end`].
#[must_use = "finish the observation with heap::end"]
pub struct Measurement {
    baseline: usize,
}

pub fn begin() -> Measurement {
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    CALLS.store(0, Relaxed);
    REQUESTED.store(0, Relaxed);
    Measurement { baseline }
}

#[derive(serde::Serialize)]
pub struct Snapshot {
    pub baseline_live_bytes: usize,
    pub peak_live_bytes: usize,
    pub peak_above_baseline_bytes: usize,
    pub stage_end_live_bytes: usize,
    pub allocation_requests: usize,
    pub requested_bytes: usize,
}

pub fn end(measurement: Measurement) -> Snapshot {
    let baseline = measurement.baseline;
    let peak = PEAK.load(Relaxed);
    // Capture on the stack; allocating report JSON must not count itself as stage work.
    Snapshot {
        baseline_live_bytes: baseline,
        peak_live_bytes: peak,
        peak_above_baseline_bytes: peak.saturating_sub(baseline),
        stage_end_live_bytes: LIVE.load(Relaxed),
        allocation_requests: CALLS.load(Relaxed),
        requested_bytes: REQUESTED.load(Relaxed),
    }
}
