//! Task event log: one JSONL file with locked compare-and-swap appends.
//!
//! `$CLUSTERVISION_HOME/tasks/events.jsonl` + sibling `events.lock`: advisory locking across
//! replay, validation, tail repair and `O_APPEND` writes (see [`crate::lockfile`]). Accepted
//! history is append-only; an incomplete final record may be removed before a successful append.
//! `write_all` can make multiple writes: a failed/crashed batch may leave a completed prefix and
//! an incomplete last record. Flush is not an fsync or a crash-atomic batch guarantee.
//!
//! # Loud, not lenient
//!
//! A chat reader may skip malformed lines; a replayed event log may not (a silently dropped
//! event changes history). Here interior garbage is **loud** (surfaced as warnings on every read)
//! and complete unrecognized final rows are loud too. Reads tolerate only an incomplete JSON
//! tail or an incomplete UTF-8 scalar at EOF. A complete but unrecognized final record refuses
//! append. Repair follows admission of the entire candidate
//! batch. A valid final event without a newline is preserved and separated from new records.
//!
//! # The CAS append
//!
//! [`TaskStore::append`] holds the lock across replay-validate-append: acquire → replay the log
//! through the reducer → apply the candidate event (any [`ReduceError`] rejects it) → append →
//! release. This is what makes `claim` a race-free first-writer-wins with zero extra lock kinds,
//! and it guarantees the log never contains an event the reducer would refuse on replay.
//!
//! # Retired kinds at the seam
//!
//! [`TaskStore::append_agent_event`] — the only path the CLI verbs and MCP tools use — refuses
//! the kinds retired in 0.15.0 ([`super::model::RETIRED_TAGS`]). The reducer itself cannot
//! (it must replay them from old logs); the store seam is where "never written again" is
//! mechanical.
//!
//! # Growth & retention (the compaction plan)
//!
//! The log is append-only and currently **unbounded**: every read replays every event ever
//! appended, so read cost grows linearly with history. Fine at fleet-task volumes for a long
//! time; not forever. The planned compaction is **snapshot + tail**: periodically fold the
//! log's stable prefix into a snapshot of the read model, keep the live tail as raw events,
//! and retire the prefix to an archive file — **preserving the audit prefix** (retired events
//! stay on disk and re-verifiable; they just stop being replayed on every read). This
//! is not implemented yet; whoever picks this up should keep the CAS append's invariant intact —
//! a snapshot the appender validates against must be exactly the fold of the retired prefix.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::lockfile::FileLock;

use super::model::{TaskEvent, TaskEventKind};
use super::reduce::{TaskReadModel, TaskReducer};

/// The log-format header record: the first line of every log this cv creates. Readers skip a
/// valid header; a header-less file is an implicit v1 (pre-header logs exist); a header with a
/// version above [`LOG_VERSION`] refuses replay loudly (written by a newer cv).
const LOG_FORMAT: &str = "cv-task-log";
const LOG_VERSION: u64 = 1;
const HEADER_LINE: &str = r#"{"format":"cv-task-log","v":1}"#;

/// Events paired with their 1-based line number in the log file — the raw material of a replay,
/// where the line number keys quarantine warnings back to the physical file.
type NumberedEvents = Vec<(usize, TaskEvent)>;

/// Physical EOF framing, separate from events admitted by replay. Readers do not modify it.
#[derive(Clone, Copy)]
enum LogTail {
    Terminated,
    NeedsNewline,
    Torn { offset: u64 },
    Unrecognized { line_no: usize },
}

/// A parsed replay of the whole log: the read model plus any loud warnings about interior
/// garbage. Every read surface must show `warnings` to its caller — a corrupted coordination log
/// is a finding, not a detail.
///
/// Read paths are **best effort + loud**: an interior event the reducer refuses is *quarantined*
/// (skipped, counted, warned about) rather than bricking every consumer at once. The append path
/// stays strict — see [`TaskStore::append`].
#[derive(Debug)]
pub struct ReplayOutcome {
    pub model: TaskReadModel,
    /// The events that actually folded into `model` (quarantined events are excluded).
    pub events: Vec<TaskEvent>,
    pub warnings: Vec<String>,
    /// Parseable events the reducer refused on replay; each also produced a warning.
    pub quarantined: usize,
}

/// Handle on a task log directory. Cheap to construct; every operation opens files fresh.
#[derive(Clone, Debug)]
pub struct TaskStore {
    dir: PathBuf,
}

/// The state validated and committed while holding the append lock. Carry it to callers rather
/// than replaying again after unlocking, when another writer may already have changed the log.
pub(super) struct Committed {
    pub events: Vec<TaskEvent>,
    pub model: TaskReadModel,
}

impl TaskStore {
    /// The default store at [`super::tasks_dir`].
    pub fn default_store() -> TaskStore {
        TaskStore {
            dir: super::tasks_dir(),
        }
    }

    /// A store rooted at `dir` (testing and embedding).
    pub fn at(dir: impl Into<PathBuf>) -> TaskStore {
        TaskStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("events.lock")
    }

    /// Replay without holding the lock. Readers may see a completed prefix of a concurrent
    /// batch and ignore an incomplete final record; this is not an atomic snapshot of a batch.
    ///
    /// Events the reducer refuses are **quarantined**: skipped with a loud warning (line number +
    /// reason) and counted, so one poisoned interior line degrades the model instead of bricking
    /// the CLI, `cv serve`, and every append at once.
    pub fn replay(&self) -> Result<ReplayOutcome> {
        let (raw, mut warnings, _) = self.read_events()?;
        let path = self.events_path();
        let mut reducer = TaskReducer::new();
        let mut events = Vec::with_capacity(raw.len());
        let mut quarantined = 0usize;
        for (line_no, ev) in raw {
            match reducer.apply(&ev) {
                Ok(()) => events.push(ev),
                Err(e) => {
                    quarantined += 1;
                    warnings.push(format!(
                        "task log {}: line {line_no} quarantined — the reducer refused it ({e}); \
                         event skipped, model is best-effort",
                        path.display()
                    ));
                }
            }
        }
        Ok(ReplayOutcome {
            model: reducer.into_model(),
            events,
            warnings,
            quarantined,
        })
    }

    /// Agent-facing append: the write-time rules ([`TaskReducer::admit`] — retired kinds, bare
    /// `decision` tags) and the CAS, in one locked step.
    pub fn append_agent_event(&self, event: TaskEvent) -> Result<TaskEvent> {
        self.append(event)
    }

    /// Validate several events in order under one lock before writing any. Candidate refusal
    /// leaves the log unchanged; I/O failure or a crash may leave a completed prefix of the batch.
    /// `cv task done --note-file F` orders the evidence note before the terminal event.
    pub fn append_agent_events(&self, events: Vec<TaskEvent>) -> Result<Vec<TaskEvent>> {
        if events.is_empty() {
            return Ok(events);
        }
        Ok(self.append_committed(events)?.events)
    }

    /// The locked CAS: replay → apply candidate → append. Returns the appended event.
    ///
    /// Unlike the read path, the append path is **strict and fails closed on degraded reads**:
    /// any read warning (interior garbage) or reducer-refused line means this binary's model of
    /// the log is incomplete — a stale binary must not write against an incomplete model, so the
    /// append is refused with the problem named instead of being validated against a lie.
    fn append(&self, event: TaskEvent) -> Result<TaskEvent> {
        let mut out = self.append_committed(vec![event])?.events;
        Ok(out.pop().expect("one event in, one out"))
    }

    /// [`Self::append`] for a batch: admit and serialize every candidate before repairing EOF
    /// framing or writing. The lock covers the entire replay, repair and append.
    pub(super) fn append_committed(&self, events: Vec<TaskEvent>) -> Result<Committed> {
        anyhow::ensure!(!crate::env::frozen(), "--frozen forbids writing tasks");
        debug_assert!(!events.is_empty());
        fs::create_dir_all(&self.dir).with_context(|| format!("creating tasks dir {}", self.dir.display()))?;
        let _lock = FileLock::acquire(self.lock_path())?;

        // Replay under the lock: the candidate is validated against the exact state it will
        // land after.
        let (raw, warnings, tail) = self.read_events()?;
        if let LogTail::Unrecognized { line_no } = tail {
            bail!(
                "refusing to append: task log {} line {line_no} is an unrecognized final record \
                — only an incomplete tail can be recovered; repair the log or upgrade cv before writing",
                self.events_path().display()
            );
        }
        if let Some(first) = warnings.first() {
            bail!(
                "refusing to append: task log {} is degraded ({} warning(s); first: {first}) — \
                 repair the log or upgrade cv before writing",
                self.events_path().display(),
                warnings.len()
            );
        }
        let mut reducer = TaskReducer::new();
        for (line_no, ev) in raw {
            reducer.apply(&ev).map_err(|e| {
                anyhow::anyhow!(
                    "refusing to append: task log {} line {line_no} fails replay ({e}) — an \
                     incomplete model must not be written against",
                    self.events_path().display()
                )
            })?;
        }
        // Each candidate is admitted (what may be written today) and applied (what it will mean)
        // against the exact state it lands after — the previous candidates of the batch included.
        for event in &events {
            reducer
                .admit(event)
                .and_then(|()| reducer.apply(event))
                .map_err(|e| anyhow::anyhow!("event rejected: {e}"))?;
        }

        // First append to a fresh log stamps the format header (older, header-less logs stay
        // valid: readers treat them as implicit v1).
        let path = self.events_path();
        let mut line = Vec::new();
        if matches!(tail, LogTail::NeedsNewline) {
            line.push(b'\n');
        }
        if !path.exists() {
            line.extend_from_slice(HEADER_LINE.as_bytes());
            line.push(b'\n');
        }
        for event in &events {
            serde_json::to_writer(&mut line, event).context("serializing task event")?;
            line.push(b'\n');
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening task log {}", self.events_path().display()))?;
        // Candidate rejection and serialization failure cannot mutate an existing log. Once
        // admitted, discard only the unparseable EOF suffix excluded from the validated history.
        if let LogTail::Torn { offset } = tail {
            f.set_len(offset)
                .with_context(|| format!("repairing incomplete task log tail {}", path.display()))?;
        }
        f.write_all(&line)
            .with_context(|| format!("appending to task log {}", self.events_path().display()))?;
        f.flush()
            .with_context(|| format!("flushing task log {}", self.events_path().display()))?;
        Ok(Committed {
            events,
            model: reducer.into_model(),
        })
    }

    /// Read raw events as `(1-based line number, event)` pairs. Interior unparseable lines are
    /// collected as warnings, including complete malformed rows without a final newline.
    /// A syntactically incomplete EOF prefix is tolerated silently. Syntax-incomplete tails carry a byte boundary for locked recovery;
    /// other unrecognized final rows refuse append. Valid unterminated records need a newline.
    /// A format header is skipped when current, and refused loudly for a newer format version.
    fn read_events(&self) -> Result<(NumberedEvents, Vec<String>, LogTail)> {
        let path = self.events_path();
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new(), LogTail::Terminated))
            }
            Err(e) => return Err(e).with_context(|| format!("opening task log {}", path.display())),
        };

        let mut events: Vec<(usize, TaskEvent)> = Vec::new();
        let mut bad: Vec<(usize, String)> = Vec::new(); // (1-based line no, snippet)
        let mut reader = BufReader::new(file);
        let mut raw = Vec::new();
        let mut line_no = 0usize;
        let mut offset = 0u64;
        let mut tail = LogTail::Terminated;
        loop {
            raw.clear();
            let n = reader
                .read_until(b'\n', &mut raw)
                .with_context(|| format!("reading task log {}", path.display()))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let start = offset;
            offset += n as u64;
            let terminated = raw.ends_with(b"\n");
            tail = if terminated {
                LogTail::Terminated
            } else {
                LogTail::NeedsNewline
            };
            let text = match std::str::from_utf8(&raw) {
                Ok(text) => text,
                Err(error) if !terminated && error.error_len().is_none() => {
                    // A valid UTF-8 prefix ending inside its final scalar is a possible torn write.
                    tail = LogTail::Torn { offset: start };
                    break;
                }
                Err(error) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
                        .with_context(|| format!("reading task log {}", path.display()));
                }
            };
            let line = text.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TaskEvent>(line) {
                Ok(ev) => events.push((line_no, ev)),
                Err(_) => {
                    if let Some(v) = header_version(line) {
                        if v > LOG_VERSION {
                            bail!(
                                "task log {} declares format v{v} (this cv reads v{LOG_VERSION}) \
                                 — the log was written by a newer cv; upgrade before touching it",
                                path.display()
                            );
                        }
                        continue; // current (or ancient) header: skip
                    }
                    if !terminated {
                        // A complete unknown event could come from a newer binary. Recover only
                        // an unfinished JSON value, never erase a row just because its event schema
                        // or syntax is unsupported. RawValue checks grammar without a Value tree.
                        tail = match serde_json::from_str::<&serde_json::value::RawValue>(line) {
                            Err(error) if error.is_eof() => LogTail::Torn { offset: start },
                            _ => {
                                bad.push((line_no, line.chars().take(80).collect()));
                                LogTail::Unrecognized { line_no }
                            }
                        };
                        break;
                    }
                    let snippet: String = line.chars().take(80).collect();
                    bad.push((line_no, snippet));
                }
            }
        }

        let warnings = bad
            .into_iter()
            .map(|(n, snippet)| {
                format!(
                    "task log {}: line {} is not a valid event (log may be corrupted): {}",
                    path.display(),
                    n,
                    snippet
                )
            })
            .collect();
        Ok((events, warnings, tail))
    }
}

/// If `line` is a log-format header record, its declared version (`v` missing reads as 0, which
/// is ≤ current and therefore skipped — keep it dumb).
fn header_version(line: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    (v.get("format")?.as_str()? == LOG_FORMAT).then(|| v.get("v").and_then(serde_json::Value::as_u64).unwrap_or(0))
}

/// Build a new event with a fresh uuid v7 id and `ts = now`. For `Opened`, pass
/// `task_id = None` — the event id becomes the task id (open identity invariant).
pub fn new_event(task_id: Option<&str>, by: &str, kind: TaskEventKind) -> TaskEvent {
    let id = uuid::Uuid::now_v7().to_string();
    TaskEvent {
        task_id: task_id.map(String::from).unwrap_or_else(|| id.clone()),
        id,
        ts: Utc::now(),
        by: by.to_string(),
        kind,
    }
}

/// Convenience: agent-facing append to the default store.
pub fn append_agent_event(task_id: Option<&str>, by: &str, kind: TaskEventKind) -> Result<TaskEvent> {
    TaskStore::default_store().append_agent_event(new_event(task_id, by, kind))
}

/// Convenience: replay the default store.
pub fn replay() -> Result<ReplayOutcome> {
    TaskStore::default_store().replay()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::TaskState;

    /// A throwaway task dir under the system temp dir, unique per test (board.rs idiom).
    fn tmp_tasks() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-task-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_kind() -> TaskEventKind {
        TaskEventKind::Opened {
            title: "demo".into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: None,
            assignee: None,
        }
    }

    fn open_task(store: &TaskStore) -> String {
        store
            .append_agent_event(new_event(None, "agent:author", open_kind()))
            .unwrap()
            .task_id
    }

    #[test]
    fn deprecated_fields_and_self_supersession_refuse_without_writing() {
        let world = cv_testkit::World::new("task-admission-fields");
        let store = TaskStore::at(world.base.join("tasks"));
        let id = open_task(&store);
        let path = store.events_path();
        let before = fs::read(&path).unwrap();
        let channel = TaskEventKind::Opened {
            title: "old channel".into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: Some("old board".into()),
            assignee: None,
        };
        for event in [
            new_event(None, "author", channel),
            new_event(
                Some(&id),
                "author",
                TaskEventKind::Done {
                    observed: None,
                    check: Some(serde_json::json!({"cmd":"old check"})),
                },
            ),
            new_event(Some(&id), "author", TaskEventKind::Superseded { by_task: id.clone() }),
        ] {
            assert!(store.append_agent_event(event).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        assert_eq!(store.replay().unwrap().model.tasks[&id].state, TaskState::Open);
    }

    #[test]
    fn append_then_replay_round_trips() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap();

        let outcome = store.replay().unwrap();
        assert!(outcome.warnings.is_empty());
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
    }

    #[test]
    fn cas_rejects_events_the_reducer_refuses() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);

        // Done on an Open task with no live revision is fine…
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Done {
                    observed: None,
                    check: None,
                },
            ))
            .unwrap();
        // …after which nothing else applies, and crucially the log did NOT grow.
        let before = store.replay().unwrap().events.len();
        let err = store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("event rejected"), "{err}");
        assert_eq!(store.replay().unwrap().events.len(), before);
    }

    /// Validation is all-or-nothing: a note then `done` are ordered; a note then a refused
    /// candidate writes neither. This does not test or promise crash-atomic publication.
    #[test]
    fn batch_validation_writes_all_or_none() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let note = |t: &str| {
            new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted {
                    text: t.into(),
                    session_ref: None,
                },
            )
        };
        let released = new_event(Some(&task), "agent:a", TaskEventKind::Released {});
        let err = store.append_agent_events(vec![note("lost?"), released]).unwrap_err();
        assert!(err.to_string().contains("event rejected"), "{err}");
        assert_eq!(
            store.replay().unwrap().events.len(),
            1,
            "nothing of the refused batch landed"
        );

        let done = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        let out = store.append_agent_events(vec![note("the evidence"), done]).unwrap();
        assert_eq!(out.len(), 2);
        let outcome = store.replay().unwrap();
        let t = &outcome.model.tasks[&task];
        assert_eq!(t.state, TaskState::Done);
        assert_eq!(t.notes.len(), 1);
        assert!(!t.notes[0].post_close, "the note preceded the close");
    }

    /// A retired kind is never written again, however it was constructed — and a log that already
    /// holds one still replays, the retired line folding to nothing.
    #[test]
    fn agent_append_rejects_retired_kinds_but_replay_still_reads_them() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let landed: TaskEventKind = serde_json::from_value(serde_json::json!({
            "event": "landed", "upstream_head": "a".repeat(40), "observed_patch_id": "b".repeat(40)
        }))
        .unwrap();
        let err = store
            .append_agent_event(new_event(Some(&task), "agent:sneaky", landed.clone()))
            .unwrap_err();
        assert!(err.to_string().contains("retired in 0.15.0"), "{err}");
        assert_eq!(store.replay().unwrap().events.len(), 1, "nothing was appended");

        // An old log with the line already in it: read, replayed, state untouched.
        let mut f = OpenOptions::new().append(true).open(store.events_path()).unwrap();
        let old = new_event(Some(&task), "cv-verify", landed);
        writeln!(f, "{}", serde_json::to_string(&old).unwrap()).unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[1].kind.tag(), "landed");
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Open);
    }

    #[test]
    fn concurrent_claims_have_exactly_one_winner() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);

        let n = 8;
        let wins: Vec<bool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let store = store.clone();
                    let task = task.clone();
                    s.spawn(move || {
                        store
                            .append_agent_event(new_event(
                                Some(&task),
                                &format!("agent:{i}"),
                                TaskEventKind::Claimed {
                                    assignee: format!("agent:{i}"),
                                },
                            ))
                            .is_ok()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(wins.iter().filter(|w| **w).count(), 1, "exactly one claim must win");
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 2); // open + the single winning claim
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
    }

    #[test]
    fn trailing_torn_line_is_tolerated_but_interior_garbage_is_loud() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted {
                    text: "hi".into(),
                    session_ref: None,
                },
            ))
            .unwrap();

        // Torn trailing line (no newline): tolerated silently.
        let path = store.events_path();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":\"trunc").unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 2);
        assert!(outcome.warnings.is_empty());

        // Interior garbage (newline-terminated junk followed by a valid event): loud.
        let mut f = OpenOptions::new().write(true).truncate(true).open(&path).unwrap();
        let good = serde_json::to_string(&new_event(None, "agent:a", open_kind())).unwrap();
        writeln!(f, "not json at all").unwrap();
        writeln!(f, "{good}").unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("line 1"), "{}", outcome.warnings[0]);
    }

    #[test]
    fn append_recovers_torn_tail_and_preserves_completed_batch_prefix() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let note = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Noted {
                text: "completed evidence 🌙".into(),
                session_ref: None,
            },
        );
        store.append_agent_event(note.clone()).unwrap();
        let path = store.events_path();
        let prefix = fs::read(&path).unwrap();
        let mut torn = prefix.clone();
        torn.extend_from_slice(b"{\"event\":\"done\",\"id\":\"unfinished");
        fs::write(&path, &torn).unwrap();
        let replay = store.replay().unwrap();
        assert!(replay.warnings.is_empty());
        assert_eq!(replay.events.len(), 2);
        assert_eq!(fs::read(&path).unwrap(), torn, "reads never repair the log");

        let done = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Done {
                observed: None,
                check: None,
            },
        );
        assert_eq!(store.append_agent_event(done.clone()).unwrap(), done);
        let mut expected = prefix;
        expected.extend_from_slice(serde_json::to_string(&done).unwrap().as_bytes());
        expected.push(b'\n');
        assert_eq!(
            fs::read(&path).unwrap(),
            expected,
            "only the excluded EOF suffix was removed"
        );
        let replay = TaskStore::at(store.dir()).replay().unwrap();
        assert!(replay.warnings.is_empty());
        assert_eq!(replay.quarantined, 0);
        assert_eq!(&replay.events[1..], &[note, done]);
        assert_eq!(replay.model.tasks[&task].state, TaskState::Done);
        assert_eq!(replay.model.tasks[&task].notes.len(), 1);
    }

    #[test]
    fn append_separates_valid_unterminated_event_and_header() {
        for header in [false, true] {
            let store = TaskStore::at(tmp_tasks());
            let opened = new_event(None, "agent:a", open_kind());
            let prefix = format!(
                "{} \t{}\r ",
                if header {
                    format!("{HEADER_LINE}\n")
                } else {
                    String::new()
                },
                serde_json::to_string(&opened).unwrap()
            );
            fs::write(store.events_path(), &prefix).unwrap();
            assert_eq!(store.replay().unwrap().events, vec![opened.clone()]);
            let note = new_event(
                Some(&opened.task_id),
                "agent:a",
                TaskEventKind::Noted {
                    text: "still separate".into(),
                    session_ref: None,
                },
            );
            store.append_agent_event(note.clone()).unwrap();
            assert_eq!(
                fs::read_to_string(store.events_path()).unwrap(),
                format!("{prefix}\n{}\n", serde_json::to_string(&note).unwrap())
            );
            let replay = store.replay().unwrap();
            assert!(replay.warnings.is_empty());
            assert_eq!(replay.quarantined, 0);
            assert_eq!(replay.events, vec![opened, note]);
        }
        let store = TaskStore::at(tmp_tasks());
        fs::write(store.events_path(), HEADER_LINE).unwrap();
        let opened = new_event(None, "agent:a", open_kind());
        store.append_agent_event(opened.clone()).unwrap();
        assert_eq!(
            fs::read_to_string(store.events_path()).unwrap(),
            format!("{HEADER_LINE}\n{}\n", serde_json::to_string(&opened).unwrap())
        );
        assert_eq!(store.replay().unwrap().events, vec![opened]);
    }

    #[test]
    fn refused_batch_does_not_repair_or_change_existing_log() {
        // Valid missing newline, torn JSON, incomplete UTF-8, and interior garbage plus a torn
        // suffix all remain byte-identical when a later candidate in the batch is refused.
        for suffix in [
            &b""[..],
            &b"{\"id\":\"torn"[..],
            &b"{\"text\":\"\xf0\x9f\x8c"[..],
            &b"garbage\n{torn"[..],
        ] {
            let store = TaskStore::at(tmp_tasks());
            let task = open_task(&store);
            let path = store.events_path();
            let mut before = fs::read(&path).unwrap();
            if suffix.is_empty() {
                assert_eq!(before.pop(), Some(b'\n'));
            } else {
                before.extend_from_slice(suffix);
            }
            fs::write(&path, &before).unwrap();
            let note = new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted {
                    text: "must not land".into(),
                    session_ref: None,
                },
            );
            let refused = new_event(Some(&task), "agent:a", TaskEventKind::Released {});
            assert!(store.append_agent_events(vec![note, refused]).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(store.replay().unwrap().events.len(), 1);
        }
    }

    #[test]
    fn incomplete_utf8_tail_is_recovered_but_invalid_utf8_is_fail_closed() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let path = store.events_path();
        let prefix = fs::read(&path).unwrap();
        let mut torn = prefix.clone();
        torn.extend_from_slice(b"{\"text\":\"\xf0\x9f\x8c");
        fs::write(&path, torn).unwrap();
        assert!(store.replay().unwrap().warnings.is_empty());
        let note = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Noted {
                text: "recovered".into(),
                session_ref: None,
            },
        );
        store.append_agent_event(note.clone()).unwrap();
        let mut expected = prefix.clone();
        expected.extend_from_slice(serde_json::to_string(&note).unwrap().as_bytes());
        expected.push(b'\n');
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(store.replay().unwrap().events.len(), 2);

        for invalid in [&b"\xff"[..], &b"{\"text\":\"\xf0\x9f\x8c\n"[..], &b"\xe2\x28\xa1"[..]] {
            let mut before = prefix.clone();
            before.extend_from_slice(invalid);
            fs::write(&path, &before).unwrap();
            assert!(store.replay().is_err());
            assert!(store.append_agent_event(note.clone()).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn unrecognized_complete_or_malformed_final_rows_refuse_append_without_erasure() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let path = store.events_path();
        let prefix = fs::read(&path).unwrap();
        for tail in [
            r#"{"event":"a-future-event","id":"future","payload":{}}"#,
            r#"{"id":42}"#,
            "not json",
            r#"{"id":"invalid",!}"#,
        ] {
            let mut before = prefix.clone();
            before.extend_from_slice(tail.as_bytes());
            fs::write(&path, &before).unwrap();
            let replay = store.replay().unwrap();
            assert_eq!(replay.events.len(), 1);
            assert_eq!(replay.warnings.len(), 1);
            assert!(replay.warnings[0].contains("line 3"));
            let error = store
                .append_agent_event(new_event(
                    Some(&task),
                    "agent:a",
                    TaskEventKind::Noted {
                        text: "must not erase unknown history".into(),
                        session_ref: None,
                    },
                ))
                .unwrap_err();
            assert!(error.to_string().contains("unrecognized final record"), "{error}");
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn concurrent_claims_recover_torn_tail_with_one_winner() {
        let store = TaskStore::at(tmp_tasks());
        let task = open_task(&store);
        let path = store.events_path();
        let mut before = fs::read(&path).unwrap();
        before.extend_from_slice(b"{\"id\":\"torn");
        fs::write(&path, before).unwrap();
        let wins = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let store = store.clone();
                    let task = task.clone();
                    scope.spawn(move || {
                        store
                            .append_agent_event(new_event(
                                Some(&task),
                                &format!("agent:{i}"),
                                TaskEventKind::Claimed {
                                    assignee: format!("agent:{i}"),
                                },
                            ))
                            .is_ok()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count()
        });
        assert_eq!(wins, 1);
        let replay = store.replay().unwrap();
        assert!(replay.warnings.is_empty());
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.model.tasks[&task].state, TaskState::Claimed);
    }

    #[test]
    fn interior_reducer_refused_event_is_quarantined_and_replay_continues() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap();

        // Hand-write a parseable event the reducer refuses (claim on an unknown task), then a
        // valid trailing event — the poisoned line must not take the rest of the log with it.
        let refused = new_event(
            Some("00000000-0000-7000-8000-00000000dead"),
            "agent:evil",
            TaskEventKind::Claimed {
                assignee: "agent:evil".into(),
            },
        );
        let good = new_event(
            Some(&task),
            "agent:a",
            TaskEventKind::Noted {
                text: "still here".into(),
                session_ref: None,
            },
        );
        let mut f = OpenOptions::new().append(true).open(store.events_path()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&refused).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&good).unwrap()).unwrap();
        drop(f);

        let outcome = store.replay().unwrap();
        assert_eq!(outcome.quarantined, 1);
        assert_eq!(outcome.events.len(), 3, "open + claim + trailing note all survive");
        assert_eq!(outcome.warnings.len(), 1);
        // Line 1 is the header, so the refused event landed on line 4.
        assert!(
            outcome.warnings[0].contains("line 4") && outcome.warnings[0].contains("quarantined"),
            "{}",
            outcome.warnings[0]
        );
        let t = &outcome.model.tasks[&task];
        assert_eq!(t.state, TaskState::Claimed);
        assert_eq!(t.notes.len(), 1, "events after the quarantined line still reduce");
    }

    #[test]
    fn append_fails_closed_on_degraded_log() {
        // Degraded by a reducer-refused interior event.
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let refused = new_event(
            Some("00000000-0000-7000-8000-00000000dead"),
            "agent:evil",
            TaskEventKind::Claimed {
                assignee: "agent:evil".into(),
            },
        );
        let mut f = OpenOptions::new().append(true).open(store.events_path()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&refused).unwrap()).unwrap();
        drop(f);
        let before = fs::read(store.events_path()).unwrap();
        let err = store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("refusing to append"), "{err}");
        assert_eq!(fs::read(store.events_path()).unwrap(), before);
        assert_eq!(
            store.replay().unwrap().events.len(),
            1,
            "the refused append wrote nothing"
        );

        // Degraded by interior garbage (a read warning): same refusal.
        let dir2 = tmp_tasks();
        let store2 = TaskStore::at(&dir2);
        let task2 = open_task(&store2);
        let mut f = OpenOptions::new().append(true).open(store2.events_path()).unwrap();
        writeln!(f, "not json at all").unwrap();
        drop(f);
        let before2 = fs::read(store2.events_path()).unwrap();
        let err = store2
            .append_agent_event(new_event(
                Some(&task2),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("refusing to append"), "{err}");
        assert_eq!(fs::read(store2.events_path()).unwrap(), before2);
    }

    #[test]
    fn header_is_written_once_and_skipped_on_read() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed {
                    assignee: "agent:a".into(),
                },
            ))
            .unwrap();

        let content = fs::read_to_string(store.events_path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], HEADER_LINE, "first line of a fresh log is the format header");
        assert_eq!(
            content.matches(LOG_FORMAT).count(),
            1,
            "the header is written exactly once, not per append"
        );

        let outcome = store.replay().unwrap();
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.events.len(), 2, "the header is not an event");
    }

    #[test]
    fn newer_format_header_refuses_replay_and_append() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let good = serde_json::to_string(&new_event(None, "agent:a", open_kind())).unwrap();
        let mut f = File::create(store.events_path()).unwrap();
        writeln!(f, "{{\"format\":\"cv-task-log\",\"v\":2}}").unwrap();
        writeln!(f, "{good}").unwrap();
        drop(f);

        let err = store.replay().unwrap_err();
        assert!(err.to_string().contains("newer cv"), "{err}");
        let err = store
            .append_agent_event(new_event(None, "agent:a", open_kind()))
            .unwrap_err();
        assert!(err.to_string().contains("newer cv"), "{err}");
    }

    #[test]
    fn replay_equals_live_across_process_restarts() {
        // Simulated restart: two distinct TaskStore values over the same dir.
        let dir = tmp_tasks();
        let task = {
            let store = TaskStore::at(&dir);
            open_task(&store)
        };
        let store2 = TaskStore::at(&dir);
        store2
            .append_agent_event(new_event(
                Some(&task),
                "agent:b",
                TaskEventKind::Claimed {
                    assignee: "agent:b".into(),
                },
            ))
            .unwrap();
        let outcome = store2.replay().unwrap();
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
        assert_eq!(
            outcome.model,
            TaskReducer::reduce(&outcome.events).unwrap(),
            "replay parity"
        );
    }
}
