//! Decode once per index, for Codex (its only user; named for it until a second harness hands
//! off). `cv index` runs discovery beside the search sweep: a compressed Codex
//! rollout that discovery decoded in full is handed, with its ref, to the sweep, which streams
//! those decoded bytes instead of decoding the file again. A window bounds the decoded rollouts
//! alive at once, by bytes and by count (discovery waits for the sweep), so temp disk holds a
//! window and the process a few descriptors per slot, never the fleet.
use crate::SessionRef;
use std::cell::RefCell;
use std::sync::{mpsc, Arc, Condvar, Mutex};

/// One rollout's decoded bytes, alive until dropped (and holding its place in the window).
pub struct Decoded {
    rollout: crate::harness::codex::Rollout,
    _permit: Permit,
}

impl Decoded {
    /// Run `f` with these bytes standing in for a decode of the same, unchanged source on this
    /// thread. A source whose identity changed since discovery decodes afresh.
    pub fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        crate::harness::codex::with_decoded(&self.rollout, f)
    }

    /// The decoded bytes held: what reading them in place of the file works over.
    pub fn decoded_bytes(&self) -> u64 {
        self.rollout.decoded_bytes()
    }
}

/// The decoded size a compressed rollout's first frame header declares, if it declares one: an
/// unverified hint for sizing admission, never for trusting the data.
pub fn decoded_size_hint(path: &std::path::Path) -> Option<u64> {
    crate::harness::store::zst::content_size(path)
}

/// Discovery's end: makes decoded rollouts available to the consumer while it is alive.
#[derive(Clone)]
pub struct Handoff {
    window: Arc<Window>,
    tx: mpsc::Sender<(SessionRef, Decoded)>,
}

/// The consumer's end. Dropping it closes the window: discovery stops waiting and stops handing.
pub struct Receiver {
    window: Arc<Window>,
    rx: mpsc::Receiver<(SessionRef, Decoded)>,
}

/// Decoded bytes and rollouts alive (counted from before each decode), and whether the consumer
/// is gone.
struct Window {
    state: Mutex<WindowState>,
    freed: Condvar,
    limit: u64,
    /// Rollouts alive at once. Each holds open descriptors (its source and its decoded copy), so
    /// a byte limit alone let a window of small rollouts exhaust the process's open files.
    slots: usize,
}

/// The window's live byte and rollout counts and whether the consumer has gone (dropped its
/// [`Receiver`]).
struct WindowState {
    live: u64,
    held: usize,
    consumer_gone: bool,
}

struct Permit(Arc<Window>, u64);

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.live -= self.1;
        state.held -= 1;
        drop(state);
        self.0.freed.notify_all();
    }
}

/// A handoff holding at most `limit` decoded bytes and `slots` rollouts at once, or one larger
/// rollout alone.
pub fn channel(limit: u64, slots: usize) -> (Handoff, Receiver) {
    let window = Arc::new(Window {
        state: Mutex::new(WindowState {
            live: 0,
            held: 0,
            consumer_gone: false,
        }),
        freed: Condvar::new(),
        limit,
        slots: slots.max(1),
    });
    let (tx, rx) = mpsc::channel();
    (
        Handoff {
            window: window.clone(),
            tx,
        },
        Receiver { window, rx },
    )
}

impl Receiver {
    /// The next decoded rollout; `None` once discovery is over.
    pub fn recv(&self) -> Option<(SessionRef, Decoded)> {
        self.rx.recv().ok()
    }
    /// The next decoded rollout already handed over, without waiting.
    pub fn try_recv(&self) -> Option<(SessionRef, Decoded)> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.window.state.lock().unwrap().consumer_gone = true;
        self.window.freed.notify_all();
    }
}

impl Handoff {
    /// Room for `bytes` and one rollout in the window (all of it when unknown), waiting while it
    /// is too full;
    /// `None` once the consumer is gone. Discovery waits here on a worker thread; the sweep frees
    /// room without discovery.
    fn reserve(&self, bytes: Option<u64>) -> Option<Permit> {
        let bytes = bytes.unwrap_or(self.window.limit);
        let mut state = self.window.state.lock().unwrap();
        while state.held != 0
            && (state.live.saturating_add(bytes) > self.window.limit || state.held >= self.window.slots)
            && !state.consumer_gone
        {
            state = self.window.freed.wait(state).unwrap();
        }
        if state.consumer_gone {
            return None;
        }
        state.live += bytes;
        state.held += 1;
        Some(Permit(self.window.clone(), bytes))
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Handoff>> = const { RefCell::new(None) };
}

/// The handoff of the discovery running on this thread, if any.
pub(crate) fn current() -> Option<Handoff> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Run `f` with `handoff` current on this thread (discovery workers carry it from their spawner).
pub(crate) fn within<T>(handoff: &Option<Handoff>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Handoff>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|current| *current.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CURRENT.with(|current| current.replace(handoff.clone())));
    f()
}

/// Decode through `decode` with room for `bytes` reserved in the current handoff's window, and
/// hand the decoded rollout over with its ref. Without a handoff this is just `decode`.
pub(crate) fn decode_and_offer(
    bytes: impl FnOnce() -> Option<u64>,
    decode: impl FnOnce() -> anyhow::Result<(SessionRef, Option<crate::harness::codex::Rollout>)>,
) -> anyhow::Result<SessionRef> {
    let Some(handoff) = current() else {
        return decode().map(|(r, _)| r);
    };
    let permit = handoff.reserve(bytes());
    let (r, rollout) = decode()?;
    if let (Some(permit), Some(rollout)) = (permit, rollout) {
        let _ = handoff.tx.send((
            r.clone(),
            Decoded {
                rollout,
                _permit: permit,
            },
        ));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full window holds discovery until the consumer frees room; an empty one admits a
    /// rollout larger than the window alone; a gone consumer releases every waiter.
    #[test]
    fn a_full_window_waits_for_freed_room_and_a_closed_one_never_waits() {
        let (handoff, receiver) = channel(10, usize::MAX);
        let first = handoff.reserve(Some(6)).expect("an empty window has room");
        let second = handoff.reserve(Some(4)).expect("room for both");
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| handoff.reserve(Some(5)).map(|permit| permit.1));
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(!waiting.is_finished(), "a full window waits");
            drop(first);
            assert_eq!(waiting.join().unwrap(), Some(5), "freed room is taken");
        });
        drop(second);
        let unknown = handoff
            .reserve(None)
            .expect("unknown size takes the empty window whole");
        assert_eq!(unknown.1, 10);
        drop(unknown);
        let _alone = handoff.reserve(Some(25)).expect("a larger rollout runs alone");
        drop(receiver);
        assert!(
            handoff.reserve(Some(1)).is_none(),
            "a closed window never waits, even full"
        );
    }

    /// Small rollouts fill the window's slots before its bytes: each holds descriptors, so the
    /// count of rollouts alive is bounded whatever their size.
    #[test]
    fn a_window_of_small_rollouts_waits_for_a_free_slot() {
        let (handoff, _receiver) = channel(1 << 30, 2);
        let first = handoff.reserve(Some(1)).expect("a free slot");
        let _second = handoff.reserve(Some(1)).expect("a free slot");
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| handoff.reserve(Some(1)).map(|permit| permit.1));
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(
                !waiting.is_finished(),
                "two slots held: the third waits, bytes to spare"
            );
            drop(first);
            assert_eq!(waiting.join().unwrap(), Some(1), "a freed slot is taken");
        });
    }
}
