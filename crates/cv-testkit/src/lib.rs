//! The integration tests' one hermetic world. Every `crates/*/tests/*.rs` file that needs a fake
//! `$HOME` builds it here, so the rules live in one place:
//!
//! - a temp `base/` holding `home/` (with `.claude/projects/-work-proj`) and `cvhome/`, removed on
//!   drop;
//! - the real binary, spawned with `HOME`, `CLUSTERVISION_HOME` and the XDG dirs pointing into it,
//!   and every variable in [`CLEARED_ENV`] removed, so an agent seat's own identity, Claude config
//!   dir or server token never leaks into a test;
//! - error-path honesty: [`World::cv_fails`] asserts a nonzero exit, a non-empty stderr and no
//!   panic.
//!
//! The env is passed only to the child, so tests run in parallel. A test that drives the library
//! in-process takes [`World::in_process`], which hands the same environment to the library as a
//! value scoped to the test's thread (`cv_core::env`), so those run in parallel too. (This crate
//! cannot name cv-core: cv-core's own unit tests would then link a second copy of it.)
//!
//! A dev-dependency only: the dependency fence scans `[dependencies]`, and `cargo publish` strips
//! a path-only dev-dependency.

pub mod heap;

use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Ambient variables no test may inherit: identity (`CV_ENDPOINT`), extra Claude roots
/// (`CLAUDE_CONFIG_DIR`, `CLUSTERVISION_CLAUDE_ROOTS`), the catalog's staleness override and
/// all harness store/export root overrides, the pack LLM switch (tests exercise the zero-network digest), and the server
/// tokens (a token in the shell must not gate a test's requests).
pub const CLEARED_ENV: &[&str] = &[
    "CV_ENDPOINT",
    "CLAUDE_CONFIG_DIR",
    "CLUSTERVISION_CLAUDE_ROOTS",
    "CLUSTERVISION_MAX_STALE_SECS",
    "CURSOR_USER_DIR",
    "CURSOR_CONFIG_DIR",
    "CODEX_HOME",
    "HERMES_HOME",
    "KIMI_SHARE_DIR",
    "KIMI_CODE_HOME",
    "CONTINUE_GLOBAL_DIR",
    "GOOSE_PATH_ROOT",
    "OPENCLAW_STATE_DIR",
    "OPENCODE_DB",
    "CLINE_SESSION_DATA_DIR",
    "CLINE_DATA_DIR",
    "CLINE_DIR",
    "CV_EXPORTS",
    "CLAURDVOYANT_MAX_STALE_SECS",
    "CV_PACK_LLM",
    "CV_SERVE_TOKEN",
    "CVD_TOKEN",
];

/// `(stdout, stderr)` of a run that succeeded or failed as asserted.
pub type Output = (String, String);

/// The `cv` binary cargo built for this test run: integration tests live in `target/<profile>/deps/`,
/// the binary one directory up.
pub fn cv_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>");
    let bin = dir.join(format!("cv{}", std::env::consts::EXE_SUFFIX));
    assert!(
        bin.is_file(),
        "no cv binary at {} (is this a crates/cv test?)",
        bin.display()
    );
    bin
}

/// A fresh, unique directory under the system temp dir.
pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cv-test-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Lines as a JSONL body, one per line, newline-terminated.
pub fn jsonl(lines: &[Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// An environment as `(name, value)` pairs.
pub type Vars = Vec<(OsString, OsString)>;

/// One temp world: a fake `$HOME` with a Claude project dir, and a `$CLUSTERVISION_HOME`.
pub struct World {
    pub base: PathBuf,
    pub home: PathBuf,
    pub cv_home: PathBuf,
    /// `home/.claude/projects/-work-proj`, where [`World::write_session`] writes.
    pub proj: PathBuf,
}

/// A [`World`] whose environment the library reads in-process while `scope` lives.
pub struct InProcess<S> {
    /// The entered environment (declared first: it is left before the world's dirs go).
    pub scope: S,
    world: World,
}

impl<S> Deref for InProcess<S> {
    type Target = World;
    fn deref(&self) -> &World {
        &self.world
    }
}

impl World {
    pub fn new(tag: &str) -> World {
        let base = temp_dir(tag);
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        let proj = home.join(".claude/projects/-work-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&cv_home).unwrap();
        World {
            base,
            home,
            cv_home,
            proj,
        }
    }

    /// A world for tests that call the library in-process. `enter` (cv-core's
    /// `Env::enter_isolated`) receives the environment [`World::command`] gives a child, and the
    /// one a thread outside every world should read (a home that does not exist), and returns
    /// the guard that keeps the first entered on this thread. The process is never mutated.
    pub fn in_process<S>(tag: &str, enter: impl FnOnce(Vars, Vars) -> S) -> InProcess<S> {
        let world = World::new(tag);
        let nowhere = std::env::temp_dir().join(format!("cv-test-unscoped-{}", std::process::id()));
        let outside = inherited_with(world_env(&nowhere.join("home"), &nowhere.join("cvhome")));
        let scope = enter(inherited_with(world.env()), outside);
        InProcess { scope, world }
    }

    /// The variables that make a process see this world.
    pub fn env(&self) -> [(&'static str, PathBuf); 9] {
        world_env(&self.home, &self.cv_home)
    }

    /// `program` in this world: cwd `base`, [`World::env`] set, [`CLEARED_ENV`] removed.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.base);
        for k in CLEARED_ENV {
            cmd.env_remove(k);
        }
        cmd.envs(self.env());
        cmd
    }

    /// `cv` in this world (see [`World::command`]).
    pub fn cv_command(&self) -> Command {
        self.command(cv_bin())
    }

    /// Run `cv`; returns (success, exit code or -1 on a signal, stdout, stderr).
    pub fn cv(&self, args: &[&str]) -> (bool, i32, String, String) {
        self.cv_full(args, &[], None)
    }

    /// [`World::cv`] with extra env vars, set after the clears.
    pub fn cv_env(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, i32, String, String) {
        self.cv_full(args, env, None)
    }

    /// [`World::cv`] with `stdin` fed to the process.
    pub fn cv_stdin(&self, args: &[&str], stdin: &str) -> (bool, i32, String, String) {
        self.cv_full(args, &[], Some(stdin))
    }

    pub fn cv_full(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> (bool, i32, String, String) {
        let mut cmd = self.cv_command();
        cmd.args(args).envs(env.iter().copied());
        let out = match stdin {
            None => cmd.stdin(Stdio::null()).output().expect("cv should run"),
            Some(text) => output_with_stdin(cmd, text),
        };
        (
            out.status.success(),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Run `cv`, asserting success.
    pub fn cv_ok(&self, args: &[&str]) -> Output {
        let (ok, code, out, err) = self.cv(args);
        assert!(ok, "cv {args:?} exited {code}\nstdout:\n{out}\nstderr:\n{err}");
        (out, err)
    }

    /// Run `cv`, asserting it fails the honest way: nonzero exit, not a signal, an explanation on
    /// stderr, and no panic.
    pub fn cv_fails(&self, args: &[&str]) -> Output {
        let (ok, code, out, err) = self.cv(args);
        assert!(!ok, "cv {args:?} unexpectedly succeeded\nstdout:\n{out}");
        assert_ne!(code, -1, "cv {args:?} died by signal (panic/abort?)\nstderr:\n{err}");
        assert!(
            !err.trim().is_empty(),
            "cv {args:?} failed silently (empty stderr)\nstdout:\n{out}"
        );
        assert!(
            !err.contains("panicked"),
            "cv {args:?} panicked instead of erroring:\n{err}"
        );
        (out, err)
    }

    /// `proj/<sid>.jsonl`, the Claude transcript of session `sid`.
    pub fn transcript(&self, sid: &str) -> PathBuf {
        self.proj.join(format!("{sid}.jsonl"))
    }

    /// Write a Claude session under `proj`; `sid` becomes the session id.
    pub fn write_session(&self, sid: &str, lines: &[Value]) -> PathBuf {
        let path = self.transcript(sid);
        fs::write(&path, jsonl(lines)).unwrap();
        path
    }

    /// A directly spawned sub-agent of `sid`: `<sid>/subagents/agent-<id>.jsonl` and its meta.
    pub fn write_agent(&self, sid: &str, agent_id: &str, meta: &Value, lines: &[Value]) -> PathBuf {
        let dir = self.proj.join(sid).join("subagents");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("agent-{agent_id}.jsonl"));
        fs::write(&path, jsonl(lines)).unwrap();
        fs::write(dir.join(format!("agent-{agent_id}.meta.json")), meta.to_string()).unwrap();
        path
    }

    /// Replace the task log with `lines` after its header (the one way to plant an OLD event:
    /// the CLI stamps `now`).
    pub fn seed_task_log(&self, lines: &[Value]) {
        let dir = self.cv_home.join("tasks");
        fs::create_dir_all(&dir).unwrap();
        let body = format!("{{\"format\":\"cv-task-log\",\"v\":1}}\n{}", jsonl(lines));
        fs::write(dir.join("events.jsonl"), body).unwrap();
    }

    /// Append raw events to the task log, after whatever the CLI wrote: how a test plants a shape
    /// the CLI no longer writes.
    pub fn append_task_log(&self, lines: &[Value]) {
        let dir = self.cv_home.join("tasks");
        fs::create_dir_all(&dir).unwrap();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))
            .unwrap();
        f.write_all(jsonl(lines).as_bytes()).unwrap();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

fn world_env(home: &Path, cv_home: &Path) -> [(&'static str, PathBuf); 9] {
    [
        ("HOME", home.to_path_buf()),
        ("USERPROFILE", home.to_path_buf()),
        ("APPDATA", home.join(".config")),
        ("LOCALAPPDATA", home.join(".local/share")),
        ("CLUSTERVISION_HOME", cv_home.to_path_buf()),
        // Linux fallbacks for dirs::cache_dir / config_dir / data_dir; harmless on macOS.
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_STATE_HOME", home.join(".local/state")),
    ]
}

/// The inherited environment less [`CLEARED_ENV`], with `env` set: what [`World::command`] gives.
fn inherited_with(env: [(&'static str, PathBuf); 9]) -> Vars {
    let mut vars: Vars = std::env::vars_os()
        .filter(|(k, _)| {
            !CLEARED_ENV.iter().any(|c| k == OsStr::new(c)) && !env.iter().any(|(e, _)| k == OsStr::new(e))
        })
        .collect();
    vars.extend(env.into_iter().map(|(k, v)| (k.into(), v.into_os_string())));
    vars
}

fn output_with_stdin(mut cmd: Command, text: &str) -> std::process::Output {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("cv should spawn");
    let mut input = child.stdin.take().unwrap();
    // Drain both output pipes while feeding stdin: a child may emit more than a pipe
    // holds before reading its next input byte. Early rejection may close stdin.
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || input.write_all(text.as_bytes()));
        let output = child.wait_with_output().expect("cv should run");
        let written = writer.join().expect("stdin writer should finish");
        if let Err(error) = written {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe,
                "stdin write failed: {error}"
            );
        }
        output
    })
}

/// Claude Code transcript records, in the shapes the real harness writes. Every record carries
/// `sessionId: "s"`; [`with`] adds or overrides fields.
pub mod claude {
    use serde_json::{json, Value};

    /// A user prompt, with `cwd` (the field `cv ls` reads the project from).
    pub fn user(uuid: &str, ts: &str, text: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "sessionId": "s", "timestamp": ts, "cwd": "/work/proj",
            "message": {"role": "user", "content": text}
        })
    }

    /// An assistant turn of `model` with these content blocks.
    pub fn assistant(uuid: &str, ts: &str, model: &str, blocks: Value) -> Value {
        json!({
            "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "message": {"role": "assistant", "model": model, "content": blocks}
        })
    }

    /// An assistant turn as Claude Code writes it for a real model: an API message id
    /// `msg_<uuid>` and a `usage` object.
    pub fn reply(uuid: &str, ts: &str, blocks: Value, usage: Value) -> Value {
        json!({
            "type": "assistant", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "message": {"role": "assistant", "model": "claude-opus-4-1-20250805", "id": format!("msg_{uuid}"),
                "content": blocks, "usage": usage}
        })
    }

    /// A `tool_use` block.
    pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    /// The user record carrying a tool's result.
    pub fn tool_result(uuid: &str, ts: &str, tool_use_id: &str, content: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]}
        })
    }

    /// A compaction boundary.
    pub fn boundary(uuid: &str, ts: &str, trigger: &str, pre_tokens: u64) -> Value {
        json!({
            "type": "system", "subtype": "compact_boundary", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "content": "Conversation compacted",
            "compactMetadata": {"trigger": trigger, "preTokens": pre_tokens, "durationMs": 1000}
        })
    }

    /// The compaction summary that seeds the next window.
    pub fn compact_summary(uuid: &str, ts: &str, body: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "isCompactSummary": true, "sessionId": "s", "timestamp": ts,
            "message": {"role": "user", "content": body}
        })
    }

    /// The `SubagentStop` hook attachment the harness writes when a child returns.
    pub fn subagent_stop(uuid: &str, ts: &str) -> Value {
        json!({
            "type": "attachment", "uuid": uuid, "sessionId": "s", "timestamp": ts,
            "attachment": {"type": "hook_success", "hookName": "SubagentStop", "hookEvent": "SubagentStop", "content": ""}
        })
    }

    /// The `queue-operation` task notification for a background child.
    pub fn notification(ts: &str, agent_id: &str, status: &str) -> Value {
        json!({
            "type": "queue-operation", "operation": "enqueue", "timestamp": ts, "sessionId": "s",
            "content": format!("<task-notification>\n<task-id>{agent_id}</task-id>\n<status>{status}</status>\n<summary>done</summary>\n</task-notification>")
        })
    }

    /// `base` with `extra`'s fields merged in, recursively for objects.
    pub fn with(mut base: Value, extra: Value) -> Value {
        merge(&mut base, extra);
        base
    }

    fn merge(base: &mut Value, extra: Value) {
        match (base, extra) {
            (Value::Object(b), Value::Object(e)) => {
                for (k, v) in e {
                    match b.get_mut(&k) {
                        Some(slot) if slot.is_object() && v.is_object() => merge(slot, v),
                        _ => {
                            b.insert(k, v);
                        }
                    }
                }
            }
            (b, e) => *b = e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_runs_drain_output_before_input_is_consumed() {
        let world = World::new("duplex-pipes");
        let mut command = world.command(std::env::current_exe().unwrap());
        command
            .args(["--exact", "tests::duplex_child", "--ignored", "--nocapture"])
            .env("CV_TESTKIT_DUPLEX_CHILD", "1");
        let output = output_with_stdin(command, &"i".repeat(2 * 1024 * 1024));
        assert!(output.status.success(), "{:?}", output.status);
        assert!(output.stdout.len() >= 1024 * 1024);
        assert!(output.stderr.len() >= 1024 * 1024);
    }

    #[test]
    fn stdin_runs_allow_a_successful_child_to_close_input_early() {
        let world = World::new("early-stdin-close");
        let mut command = world.command(std::env::current_exe().unwrap());
        command
            .args(["--exact", "tests::duplex_child", "--ignored", "--nocapture"])
            .env("CV_TESTKIT_DUPLEX_CHILD", "discard");
        let output = output_with_stdin(command, &"i".repeat(2 * 1024 * 1024));
        assert!(output.status.success(), "{:?}", output.status);
    }

    #[test]
    #[ignore = "subprocess fixture for stdin_runs_drain_output_before_input_is_consumed"]
    fn duplex_child() {
        if std::env::var("CV_TESTKIT_DUPLEX_CHILD").as_deref() == Ok("discard") {
            return;
        }
        assert_eq!(std::env::var("CV_TESTKIT_DUPLEX_CHILD").as_deref(), Ok("1"));
        std::io::stdout().write_all(&vec![b'o'; 1024 * 1024]).unwrap();
        std::io::stderr().write_all(&vec![b'e'; 1024 * 1024]).unwrap();
        let mut input = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut input).unwrap();
        assert_eq!(input, vec![b'i'; 2 * 1024 * 1024]);
    }
}
