//! The environment cv reads: `HOME`, `CLUSTERVISION_HOME`, the XDG dirs and every harness root
//! override, as one value. Library code reads it here ([`var_os`], [`home_dir`], ... are the
//! drop-in forms of `std::env` and `dirs` over [`Env::current`]), never from the process.
//!
//! A process resolves its environment once, on first read (the CLI, MCP and `cv serve` all start
//! there), so the binary is configured by its environment exactly as before. A caller that wants
//! another environment for some work enters one for that work ([`Env::enter`]) instead of mutating
//! the process: that is how in-process tests get a hermetic home each and run in parallel. The
//! scope is per thread; the library's own worker threads carry it ([`carry`], [`Env::scoped`]).

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env::VarError;
use std::ffi::{OsStr, OsString};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// A resolved environment: the variables, plus the home and cache dirs derived from them.
#[derive(Clone, Default)]
pub struct Env {
    vars: HashMap<OsString, OsString>,
    home: Option<PathBuf>,
    cache: Option<PathBuf>,
    frozen: bool,
    /// What the library keeps alive while this environment is in use: its current catalog's scan
    /// cache. Shared by the copies [`Scope::set`] makes, so a variable change keeps it.
    retained: Arc<Mutex<Option<Arc<dyn Any + Send + Sync>>>>,
}

static PROCESS: OnceLock<Arc<Env>> = OnceLock::new();

thread_local! {
    static SCOPED: RefCell<Option<Arc<Env>>> = const { RefCell::new(None) };
}

impl Env {
    /// The process environment, with `dirs`' home and cache dirs.
    pub fn from_process() -> Env {
        Env {
            vars: std::env::vars_os().collect(),
            home: dirs::home_dir(),
            cache: dirs::cache_dir(),
            frozen: std::env::var_os("CV_FROZEN").is_some_and(|v| v == "1"),
            retained: Default::default(),
        }
    }

    /// Exactly these variables and nothing inherited; home and cache derive from them as `dirs`
    /// derives them from a process environment.
    pub fn from_vars<K: Into<OsString>, V: Into<OsString>>(vars: impl IntoIterator<Item = (K, V)>) -> Env {
        let mut env = Env::default();
        for (k, v) in vars {
            env.vars.insert(k.into(), v.into());
        }
        env.derive();
        env
    }

    /// Disable store writes, including in child commands inheriting this environment.
    pub fn with_frozen(mut self) -> Self {
        self.set("CV_FROZEN", Some(OsStr::new("1")));
        self
    }

    fn derive(&mut self) {
        self.frozen = self.vars.get(OsStr::new("CV_FROZEN")).is_some_and(|v| v == "1");
        let var = |k: &str| self.var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = if cfg!(windows) { var("USERPROFILE") } else { var("HOME") };
        let cache = if cfg!(target_os = "macos") {
            home.as_ref().map(|h| h.join("Library/Caches"))
        } else if cfg!(windows) {
            var("LOCALAPPDATA")
        } else {
            var("XDG_CACHE_HOME")
                .filter(|p| p.is_absolute())
                .or_else(|| home.as_ref().map(|h| h.join(".cache")))
        };
        self.home = home;
        self.cache = cache;
    }

    /// Set (`Some`) or remove (`None`) one variable, re-deriving home and cache.
    fn set(&mut self, key: impl Into<OsString>, value: Option<&OsStr>) {
        let key = key.into();
        match value {
            Some(v) => self.vars.insert(key, v.to_owned()),
            None => self.vars.remove(&key),
        };
        self.derive();
    }

    /// As `std::env::var_os`.
    pub fn var_os(&self, key: &str) -> Option<OsString> {
        self.vars.get(OsStr::new(key)).cloned()
    }

    /// As `std::env::var`.
    pub fn var(&self, key: &str) -> Result<String, VarError> {
        match self.vars.get(OsStr::new(key)) {
            None => Err(VarError::NotPresent),
            Some(v) => v
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| VarError::NotUnicode(v.clone())),
        }
    }

    /// As `dirs::home_dir` (which it is, for the process environment).
    pub fn home_dir(&self) -> Option<PathBuf> {
        self.home.clone()
    }

    /// As `dirs::cache_dir` (which it is, for the process environment).
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.cache.clone()
    }

    pub(crate) fn retained(&self) -> &Mutex<Option<Arc<dyn Any + Send + Sync>>> {
        &self.retained
    }

    /// `$XDG_CONFIG_HOME` when set and non-empty, else `~/.config`.
    pub fn config_home(&self) -> Option<PathBuf> {
        self.var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.home_dir().map(|h| h.join(".config")))
    }

    /// `$XDG_DATA_HOME` when set to an absolute path (the spec ignores a relative value), else
    /// `~/.local/share`.
    pub fn data_home(&self) -> Option<PathBuf> {
        self.var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| self.home_dir().map(|h| h.join(".local/share")))
    }

    /// `$CLUSTERVISION_HOME`, else `~/.clustervision`: cv's own state (tasks, lanes, roots list).
    pub fn cv_home(&self) -> Option<PathBuf> {
        self.var_os("CLUSTERVISION_HOME")
            .map(PathBuf::from)
            .or_else(|| self.home_dir().map(|h| h.join(".clustervision")))
    }

    /// The environment this thread reads: the innermost entered one, else the process's.
    pub fn current() -> Arc<Env> {
        Env::scoped().unwrap_or_else(Env::process)
    }

    /// The process environment, resolved on first use unless [`Env::install`] set it first.
    pub fn process() -> Arc<Env> {
        PROCESS.get_or_init(|| Arc::new(Env::from_process())).clone()
    }

    /// Make `env` the process environment, if nothing has read it yet. Test harnesses install a
    /// home that does not exist, so a thread that was not handed its scope reads nobody's real one.
    pub fn install(env: Env) -> bool {
        PROCESS.set(Arc::new(env)).is_ok()
    }

    /// The environment entered on this thread, if any: what a spawned worker must carry.
    pub fn scoped() -> Option<Arc<Env>> {
        SCOPED.with(|s| s.borrow().clone())
    }

    /// Read `self` on this thread until the returned guard drops.
    pub fn enter(self) -> Scope {
        Scope::enter(Arc::new(self))
    }

    /// Enter on a spawned thread what [`Env::scoped`] captured on its spawner.
    pub fn reenter(env: Arc<Env>) -> Scope {
        Scope::enter(env)
    }

    /// A test harness's entry: enter `vars` on this thread, and make `outside` the process
    /// environment if nothing has read it yet, so a thread the test forgot to carry its scope into
    /// reads a home that does not exist rather than the real one.
    pub fn enter_isolated(vars: Vec<(OsString, OsString)>, outside: Vec<(OsString, OsString)>) -> Scope {
        Env::install(Env::from_vars(outside));
        Env::from_vars(vars).enter()
    }
}

/// `Command::new(program)` with the entered environment, if any, as the child's whole environment
/// (a child inherits the process's otherwise, which is the entered one only outside a scope).
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    if let Some(env) = Env::scoped() {
        cmd.env_clear().envs(&env.vars);
    }
    cmd
}

/// An entered environment; dropping it restores the previous one. Bound to its thread.
#[must_use = "the environment is left when the scope drops"]
pub struct Scope {
    prev: Option<Arc<Env>>,
    _thread: PhantomData<*const ()>,
}

impl Scope {
    fn enter(env: Arc<Env>) -> Scope {
        let prev = SCOPED.with(|s| s.borrow_mut().replace(env));
        Scope {
            prev,
            _thread: PhantomData,
        }
    }

    /// Set one variable of the entered environment (a test's mid-body `set_var`).
    pub fn set(&self, key: &str, value: impl AsRef<OsStr>) {
        Scope::update(key, Some(value.as_ref()));
    }

    /// Remove one variable of the entered environment.
    pub fn remove(&self, key: &str) {
        Scope::update(key, None);
    }

    fn update(key: &str, value: Option<&OsStr>) {
        SCOPED.with(|s| {
            let mut slot = s.borrow_mut();
            let mut env = slot.as_deref().expect("updating an entered environment").clone();
            env.set(key, value);
            *slot = Some(Arc::new(env));
        });
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SCOPED.with(|s| *s.borrow_mut() = prev);
    }
}

/// Whether this command must leave cv’s stores untouched (`--frozen` or `CV_FROZEN=1`).
/// Resolved once with the environment; scoped callers and their workers share the same policy.
pub fn frozen() -> bool {
    Env::current().frozen
}

/// `std::env::var_os` over [`Env::current`].
pub fn var_os(key: &str) -> Option<OsString> {
    Env::current().var_os(key)
}

/// `std::env::var` over [`Env::current`].
pub fn var(key: &str) -> Result<String, VarError> {
    Env::current().var(key)
}

/// `dirs::home_dir` over [`Env::current`].
pub fn home_dir() -> Option<PathBuf> {
    Env::current().home_dir()
}

/// `dirs::cache_dir` over [`Env::current`].
pub fn cache_dir() -> Option<PathBuf> {
    Env::current().cache_dir()
}

/// The third channel of the skip policy (`harness::Skipped` names all three): an accelerator
/// (the catalog, a cache, an index) that cannot open DEGRADES ONCE. The first failure of `what`
/// in this process prints one stderr line and cv carries on without it; later failures of the
/// same `what` print nothing. Nothing is memoized but the line: the caller retries the open next
/// time, so an accelerator that recovers serves again. Returns whether this call printed.
pub fn warn_once(what: &'static str, why: impl std::fmt::Display) -> bool {
    static WARNED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut warned = WARNED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.contains(&what) {
        return false;
    }
    warned.push(what);
    eprintln!("cv: {what} unavailable, continuing without it: {why}");
    true
}

/// `f` wrapped to run in the caller's entered environment, for a thread the library spawns.
pub fn carry<R>(f: impl FnOnce() -> R) -> impl FnOnce() -> R {
    let env = Env::scoped();
    move || {
        let _scope = env.map(Env::reenter);
        f()
    }
}

/// Run `f` in `env` when one was captured with [`Env::scoped`]; the rayon fan-outs' per-item form
/// of [`carry`].
pub(crate) fn within<R>(env: &Option<Arc<Env>>, f: impl FnOnce() -> R) -> R {
    match env {
        Some(env) => {
            let _scope = Env::reenter(env.clone());
            f()
        }
        None => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn scopes_nest_and_restore_and_carry_into_threads() {
        let outer = Env::from_vars([("HOME", "/a")]).enter();
        assert_eq!(home_dir().as_deref(), Some(Path::new("/a")));
        {
            let _inner = Env::from_vars([("HOME", "/b")]).enter();
            assert_eq!(home_dir().as_deref(), Some(Path::new("/b")));
            let seen = std::thread::spawn(carry(home_dir)).join().unwrap();
            assert_eq!(seen.as_deref(), Some(Path::new("/b")));
        }
        assert_eq!(home_dir().as_deref(), Some(Path::new("/a")));
        outer.set("CLUSTERVISION_HOME", "/cv");
        assert_eq!(Env::current().cv_home(), Some(PathBuf::from("/cv")));
        drop(outer);
        assert!(Env::scoped().is_none());
    }

    #[test]
    fn frozen_is_resolved_with_the_environment_and_carried() {
        let _outer = Env::from_vars([("CV_FROZEN", "0")]).enter();
        assert!(!frozen());
        {
            let _inner = Env::from_vars([("CV_FROZEN", "1")]).enter();
            assert!(frozen());
            assert!(std::thread::spawn(carry(frozen)).join().unwrap());
        }
        assert!(!frozen());
        let _flag = Env::default().with_frozen().enter();
        assert!(frozen());
        assert_eq!(var("CV_FROZEN").unwrap(), "1");
    }

    #[test]
    fn warn_once_prints_one_line_per_accelerator() {
        assert!(warn_once("env test accelerator", "first"));
        assert!(!warn_once("env test accelerator", "second"));
        assert!(warn_once("env test other accelerator", "first"));
    }
}
