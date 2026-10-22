//! Decode once per index, for Codex (its only user; named for it until a second harness hands
//! off). `cv index` runs discovery beside the search sweep: a compressed Codex
//! rollout that discovery decoded in full is handed, with its ref, to the sweep, which streams
//! those decoded bytes instead of decoding the file again. A window bounds the decoded bytes
//! alive at once (discovery waits for the sweep), so temp disk holds a window, never the fleet.
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

/// Decoded bytes alive (counted from before each decode), and whether the consumer is gone.
struct Window {
    state: Mutex<(u64, bool)>,
    freed: Condvar,
    limit: u64,
}

struct Permit(Arc<Window>, u64);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().0 -= self.1;
        self.0.freed.notify_all();
    }
}

/// A handoff holding at most `limit` decoded bytes at once, or one larger rollout alone.
pub fn channel(limit: u64) -> (Handoff, Receiver) {
    let window = Arc::new(Window {
        state: Mutex::new((0, false)),
        freed: Condvar::new(),
        limit,
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
        self.window.state.lock().unwrap().1 = true;
        self.window.freed.notify_all();
    }
}

impl Handoff {
    /// Room for `bytes` in the window (all of it when unknown), waiting while it is too full;
    /// `None` once the consumer is gone. Discovery waits here on a worker thread; the sweep frees
    /// room without discovery.
    fn reserve(&self, bytes: Option<u64>) -> Option<Permit> {
        let bytes = bytes.unwrap_or(self.window.limit);
        let mut state = self.window.state.lock().unwrap();
        while state.0 != 0 && state.0.saturating_add(bytes) > self.window.limit && !state.1 {
            state = self.window.freed.wait(state).unwrap();
        }
        if state.1 {
            return None;
        }
        state.0 += bytes;
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
        let (handoff, receiver) = channel(10);
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
}
