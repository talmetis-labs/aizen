//! Process-tree containment and tree-kill for every child Aizen spawns through a shell.
//!
//! The problem this exists to solve, measured on Windows 11 (26200): we spawn `cmd /C … & <command>`,
//! so the process we hold a handle to is the **wrapper**, not the real work. `Child::kill()` calls
//! `TerminateProcess` on that wrapper only — a grandchild (`cargo`, `node`, `pytest`, a dev server)
//! keeps running, orphaned. Worse, it inherited the write end of our stdout/stderr pipes, so a
//! subsequent `read_to_end` on those pipes **never sees EOF**: the reader thread blocks for as long
//! as the orphan lives, which for a wedged process means forever. A reproduction confirmed both
//! halves: after `kill()` the grandchild was still alive, and the drain-thread join was still blocked
//! 12s later, having been given a 45s sleeper to outlast.
//!
//! The fix is the standard one for a Windows process supervisor, already used here for language
//! servers (`agent::lsp::jobobject`): put the wrapper in a **Job Object** with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so terminating the job — or merely dropping the handle, or
//! crashing — reaps every descendant. Unix gets the equivalent via a **process group**: `setsid` at
//! spawn, then `killpg`.
//!
//! Legacy containment is best-effort: failed job creation yields a [`Containment::None`] that still
//! kills the direct child. Protected spawn instead assigns a suspended child before execution and
//! fails closed, returning owned child/containment when cleanup cannot be verified.

use std::process::{Child, Command};
use std::time::Duration;

/// `CREATE_NO_WINDOW` (winbase.h): a spawned console child (git.exe, cmd.exe, a build tool) gets no
/// console window of its own. Applied to every child Aizen spawns through [`prepare`] /
/// [`prepare_tokio`]. Two reasons, both load-bearing:
///
///  * No console flash when Aizen runs under a TUI/alt-screen or fully headless (`aizen serve`).
///  * Every console child consumes the window station's shared **desktop heap**. A fan-out of git
///    checkpoints + subagents can exhaust it, and `CreateProcess` then fails during loader init with
///    `STATUS_DLL_INIT_FAILED` (0xc0000142) — the modal "unable to start correctly" dialog. A
///    windowless child sidesteps that allocation entirely.
///
/// Output is captured through pipes regardless, so no console is ever needed. Same flag the LSP
/// client already uses (`agent::lsp::server`).
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A handle keeping a spawned tree contained. Hold it for as long as the child may live; dropping it
/// on Windows kills the whole tree (kill-on-close), which is also the crash-safety property.
pub enum Containment {
    /// Windows job object (kill-on-close). Owns the handle; closed exactly once on drop.
    #[cfg(windows)]
    Job(windows_job::Job),
    /// Unix process group led by the child (`setsid`), killed via `killpg`.
    #[cfg(unix)]
    Group(i32),
    /// Containment unavailable — [`kill_tree`] falls back to killing the direct child only.
    None,
}

impl std::fmt::Debug for Containment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(windows)]
            Self::Job(_) => f.write_str("Containment::Job"),
            #[cfg(unix)]
            Self::Group(pgid) => write!(f, "Containment::Group({pgid})"),
            Self::None => f.write_str("Containment::None"),
        }
    }
}

impl Containment {
    /// Did containment actually take? Callers use this only for diagnostics — behaviour degrades
    /// gracefully either way.
    pub fn is_contained(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Read-only proof that the containment we created holds no live process. A Windows job object
    /// accounts for every descendant; a Unix process group is the containment boundary we actually
    /// establish (`setsid` at spawn), so an empty group is the strongest proof available there. A
    /// descendant that deliberately leaves that group is outside the containment boundary — no
    /// portable mechanism observes it — and absent containment is likewise unknown. Callers must
    /// retain physical-effect ownership for `None` or errors rather than trusting wrapper exit.
    pub(crate) fn known_quiescent(&self) -> std::io::Result<Option<bool>> {
        match self {
            #[cfg(windows)]
            Self::Job(job) => job.known_quiescent(),
            #[cfg(unix)]
            Self::Group(pgid) if *pgid > 0 => Ok(Some(group_is_empty(*pgid))),
            // A non-positive group id cannot be one we created; never signal the caller's own group.
            #[cfg(unix)]
            Self::Group(_) => Ok(None),
            Self::None => Ok(None),
        }
    }
}

/// Prepare `cmd` so the child it spawns can lead its own process group (Unix). On Windows this sets
/// NO_WINDOW; legacy containment attaches after spawn, while protected spawn suspends before assign.
pub fn prepare(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and is the documented way to detach a child into a
        // new session/group from the post-fork, pre-exec hook. It touches no allocator or lock.
        // A failure fails the spawn: without a group of its own, `Containment::Group(child.id())`
        // would name a group that does not exist, and settlement would read as proven while the
        // child still runs.
        unsafe {
            cmd.pre_exec(libc_setsid);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Merge, don't clobber: `creation_flags` REPLACES the whole flag word, but no caller of
        // `prepare` sets other flags first, so a plain set is correct here (the LSP path sets its
        // own flags and does not call `prepare`).
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

/// Contain a just-spawned child. Pair with [`prepare`] on the same `Command`.
pub fn contain(child: &Child) -> Containment {
    #[cfg(windows)]
    {
        return windows_job::contain(child);
    }
    #[cfg(unix)]
    {
        // `prepare` made the child a session leader, so its pid IS the process-group id.
        return Containment::Group(child.id() as i32);
    }
    #[allow(unreachable_code)]
    {
        let _ = child;
        Containment::None
    }
}

/// Failure from the protected spawn path. A verified cleanup owns no remaining child. An unverified
/// cleanup RETURNS ownership rather than leaking a suspended process or claiming its effects settled;
/// callers must retain opaque admission with `child`/`containment` until reconciliation/cleanup.
#[derive(Debug)]
pub(crate) struct ContainedSpawnError<C> {
    pub(crate) cause: std::io::Error,
    pub(crate) cleanup_verified: bool,
    // Retention-only ownership; whole error is quarantined until exit.
    #[allow(dead_code)]
    pub(crate) child: Option<Box<std::sync::Mutex<C>>>,
    // Retention-only ownership; whole error is quarantined until exit.
    #[allow(dead_code)]
    pub(crate) containment: Option<Containment>,
}

impl<C> ContainedSpawnError<C> {
    pub(crate) fn cleanup_verified(&self) -> bool {
        self.cleanup_verified
    }

    fn before_spawn(cause: std::io::Error) -> Self {
        Self {
            cause,
            cleanup_verified: true,
            child: None,
            containment: None,
        }
    }
}

impl<C> std::fmt::Display for ContainedSpawnError<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "protected child spawn failed: {}; cleanup {}",
            self.cause,
            if self.cleanup_verified {
                "verified"
            } else {
                "UNKNOWN; child ownership retained"
            }
        )
    }
}
impl<C: std::fmt::Debug> std::error::Error for ContainedSpawnError<C> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Spawn from an already sandbox-prepared command. Windows cannot execute the new child's initial
/// thread before checked Job assignment and resume. `flags` are the caller's complete prior Windows
/// flags: stable Command exposes no getter, so never infer them from Debug output. Non-Windows keeps
/// the existing process-group spawn and returns UNKNOWN tree proof.
pub(crate) fn spawn_contained_std(
    command: &mut Command,
    #[cfg_attr(not(windows), allow(unused_variables))] flags: u32,
    #[cfg(windows)] limits: Option<&windows_job::JobLimits>,
) -> Result<(Child, Containment), ContainedSpawnError<Child>> {
    #[cfg(windows)]
    {
        use std::os::windows::{io::AsRawHandle, process::CommandExt};
        command.creation_flags(
            flags | CREATE_NO_WINDOW | windows_sys::Win32::System::Threading::CREATE_SUSPENDED,
        );
        let spawned = command.spawn();
        command.creation_flags(flags | CREATE_NO_WINDOW);
        let child = spawned.map_err(ContainedSpawnError::before_spawn)?;
        match windows_job::protect_suspended(child.as_raw_handle().cast(), child.id(), limits) {
            Ok(containment) => Ok((child, containment)),
            Err(failure) => Err(ContainedSpawnError {
                cause: failure.cause,
                cleanup_verified: failure.cleanup_verified,
                child: (!failure.cleanup_verified).then(|| Box::new(std::sync::Mutex::new(child))),
                containment: (!failure.cleanup_verified).then_some(failure.containment),
            }),
        }
    }
    #[cfg(not(windows))]
    {
        let child = command.spawn().map_err(ContainedSpawnError::before_spawn)?;
        let containment = contain(&child);
        Ok((child, containment))
    }
}

/// Tokio counterpart; the synchronous spawn/Job/resume interval never awaits and never grants
/// execution before Job assignment. Sandbox preparation must precede this call.
pub(crate) fn spawn_contained_tokio(
    command: &mut tokio::process::Command,
    #[cfg_attr(not(windows), allow(unused_variables))] flags: u32,
    #[cfg(windows)] limits: Option<&windows_job::JobLimits>,
) -> Result<(tokio::process::Child, Containment), ContainedSpawnError<tokio::process::Child>> {
    #[cfg(windows)]
    {
        command.creation_flags(
            flags | CREATE_NO_WINDOW | windows_sys::Win32::System::Threading::CREATE_SUSPENDED,
        );
        let spawned = command.spawn();
        command.creation_flags(flags | CREATE_NO_WINDOW);
        let child = spawned.map_err(ContainedSpawnError::before_spawn)?;
        let (Some(handle), Some(pid)) = (child.raw_handle(), child.id()) else {
            // A successful, unreaped Tokio child normally has both. Do not panic/drop ownership if
            // a future backend ever violates that assumption; the caller must reconcile UNKNOWN.
            return Err(ContainedSpawnError {
                cause: std::io::Error::other("new suspended Tokio child lacks process identity"),
                cleanup_verified: false,
                child: Some(Box::new(std::sync::Mutex::new(child))),
                containment: Some(Containment::None),
            });
        };
        match windows_job::protect_suspended(handle.cast(), pid, limits) {
            Ok(containment) => Ok((child, containment)),
            Err(failure) => Err(ContainedSpawnError {
                cause: failure.cause,
                cleanup_verified: failure.cleanup_verified,
                child: (!failure.cleanup_verified).then(|| Box::new(std::sync::Mutex::new(child))),
                containment: (!failure.cleanup_verified).then_some(failure.containment),
            }),
        }
    }
    #[cfg(not(windows))]
    {
        let child = command.spawn().map_err(ContainedSpawnError::before_spawn)?;
        let containment = contain_tokio(&child);
        Ok((child, containment))
    }
}

/// Kill the child **and every descendant**, then reap the direct child so it is not left a zombie.
///
/// Order matters: terminate the tree first, then `wait()`. Reaping first would leave the orphans
/// running with our pipe handles still open — the exact hang this module exists to prevent.
pub fn kill_tree(child: &mut Child, containment: &Containment) {
    terminate_tree(containment);
    let _ = child.kill();
    let _ = child.wait();
}

/// Kill every process in the containment WITHOUT touching a `Child` handle.
///
/// The async spawn path (verify gate) hands its child to `wait_with_output()`, which consumes it, so
/// there is no `Child` left to kill when the timeout fires — only `kill_on_drop`, which reaps the
/// wrapper alone. This kills the tree the wrapper left behind.
pub fn terminate_tree(containment: &Containment) {
    match containment {
        #[cfg(windows)]
        Containment::Job(job) => job.terminate(),
        #[cfg(unix)]
        Containment::Group(pgid) => {
            // SAFETY: a plain kill(2) on a process-group id we created; an invalid/exited group is
            // simply an ESRCH error we ignore.
            unsafe { libc_killpg(*pgid, 9) };
        }
        Containment::None => {}
    }
}

/// Join a pipe-drain thread with a deadline, returning `(text, timed_out)`.
///
/// `JoinHandle::join` has no timeout, so the wait moves onto a channel: the drain thread's result
/// arrives through `tx`, and a lapsed `recv_timeout` means the thread is still parked on a pipe that
/// nobody is going to close. We then abandon it — a detached thread blocked in `read` costs one stack
/// and exits if EOF ever arrives, which beats blocking the caller forever.
pub fn join_drain<T: Send + Default + 'static>(
    handle: std::thread::JoinHandle<T>,
    deadline: Duration,
) -> (T, bool) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.join().unwrap_or_default());
    });
    match rx.recv_timeout(deadline) {
        Ok(value) => (value, false),
        Err(_) => (T::default(), true),
    }
}

/// Complete-tree and actual-worker settlement, independent of wrapper exit/output completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundedSettlement {
    Proven,
    Unknown,
}

#[derive(Debug)]
pub(crate) struct BoundedExecutionError {
    pub(crate) cause: std::io::Error,
    pub(crate) settlement: BoundedSettlement,
}
impl std::fmt::Display for BoundedExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bounded child execution failed: {}; settlement {:?}",
            self.cause, self.settlement
        )
    }
}
impl std::error::Error for BoundedExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

pub(crate) type BoundedOwner = Box<dyn Send + Sync + 'static>;
static BOUNDED_QUARANTINE: std::sync::Mutex<Vec<BoundedOwner>> = std::sync::Mutex::new(Vec::new());
static BOUNDED_UNKNOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Only strict bounded bodies latch this; legacy serial UNKNOWN results never block new work.
pub(crate) fn check_bounded_effects() -> Result<(), BoundedExecutionError> {
    if BOUNDED_UNKNOWN.load(std::sync::atomic::Ordering::Acquire) {
        Err(BoundedExecutionError {
            cause: std::io::Error::other(
                "a bounded child has unverified physical effects until CLI exit",
            ),
            settlement: BoundedSettlement::Unknown,
        })
    } else {
        Ok(())
    }
}

pub(crate) fn latch_bounded_unknown(strict: bool) {
    if strict {
        BOUNDED_UNKNOWN.store(true, std::sync::atomic::Ordering::Release);
        crate::core::recovery::mark_effects_unsettled();
    }
}

/// Retain uncertain physical owners until CLI exit; this never grants admission to another body.
pub(crate) fn quarantine_bounded_owner(owner: BoundedOwner) {
    BOUNDED_QUARANTINE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(owner);
}

struct BoundedOwnership {
    _child: std::sync::Arc<std::sync::Mutex<Child>>,
    _containment: std::sync::Arc<Containment>,
    _caller: Option<BoundedOwner>,
    strict_unknown: bool,
}
fn quarantine_bounded_body(owner: BoundedOwnership) {
    if owner.strict_unknown {
        latch_bounded_unknown(true);
        quarantine_bounded_owner(Box::new(owner));
    }
    // Legacy serial callers release only after actual worker tokens finish, but do not claim proof.
}
type BoundedLifetime = crate::agent::process::SettlementLifetime<BoundedOwnership>;

/// What [`output_bounded`] came back with.
pub struct BoundedOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the command was killed at the deadline.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// A drain thread was still blocked at the grace deadline, so the text above may be short.
    pub output_truncated: bool,
    pub(crate) settlement: BoundedSettlement,
}

/// What [`output_bounded_bytes`] came back with — identical to [`BoundedOutput`] but BYTE-EXACT.
///
/// Required by callers whose stdout is binary and must survive verbatim: `git cat-file blob` hands
/// back file contents, and decoding those through `String::from_utf8_lossy` would replace every
/// invalid byte with U+FFFD — silently corrupting any non-UTF-8 file a checkpoint restores or diffs.
/// `-z`-separated git output has the same requirement (NUL-delimited, byte-exact paths).
pub struct BoundedBytes {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when the command was killed at the deadline.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// A drain thread was still blocked at the grace deadline, so the output above may be short.
    pub output_truncated: bool,
    pub(crate) settlement: BoundedSettlement,
}

impl BoundedBytes {
    /// Did the command exit successfully? (`false` for a timeout kill, which has no code — the
    /// distinction matters: a killed child must never read as success.)
    ///
    /// Currently exercised only by this module's tests; production callers rebuild a real
    /// `ExitStatus` from `code` instead (see `timemachine::run_git_bounded`).
    #[allow(dead_code)]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `cmd` to completion under a wall-clock deadline, with the whole process tree contained.
///
/// This is the safe replacement for `Command::output()`, which has **no** timeout: it blocks until
/// every pipe reaches EOF, and a grandchild that outlives its wrapper keeps the write end open, so a
/// single wedged command hangs the caller for good. Used by the interactive `!cmd` escape and by
/// custom-command `` !`cmd` `` expansion — both run on the REPL's own thread, where a hang freezes
/// the UI rather than just one tool call.
///
/// The caller owns `cmd` (working dir, env); stdio is configured here.
pub fn output_bounded(
    cmd: &mut Command,
    timeout: Duration,
    drain_grace: Duration,
) -> std::io::Result<BoundedOutput> {
    let raw = output_bounded_bytes(cmd, timeout, drain_grace)?;
    Ok(BoundedOutput {
        stdout: String::from_utf8_lossy(&raw.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&raw.stderr).into_owned(),
        code: raw.code,
        timed_out: raw.timed_out,
        output_truncated: raw.output_truncated,
        settlement: raw.settlement,
    })
}

/// [`output_bounded`] without the lossy decode: stdout/stderr come back as raw bytes.
///
/// Use this whenever the child's stdout is binary or delimiter-exact (`git cat-file blob`, any `-z`
/// output). [`output_bounded`] is a thin lossy wrapper over this function, so both share one
/// containment/deadline implementation — a fix here reaches every bounded caller.
pub fn output_bounded_bytes(
    cmd: &mut Command,
    timeout: Duration,
    drain_grace: Duration,
) -> std::io::Result<BoundedBytes> {
    output_bounded_bytes_fed(cmd, None, timeout, drain_grace)
}

fn output_bounded_bytes_fed(
    cmd: &mut Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    drain_grace: Duration,
) -> std::io::Result<BoundedBytes> {
    check_bounded_effects().map_err(std::io::Error::other)?;
    configure_bounded_stdio(cmd, input.is_some());
    prepare(cmd);
    // Never acquire another physical gate under native/TM admission. Strict core callers retain
    // their current context (activity/inherited writer ancestry) with the actual worker instead.
    let strict = crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED;
    let owner = if strict {
        crate::core::exec_ctx::current().map(|ctx| Box::new(ctx) as BoundedOwner)
    } else {
        None
    };
    let (child, containment) = match spawn_contained_std(
        cmd,
        0,
        #[cfg(windows)]
        None,
    ) {
        Ok(spawned) => spawned,
        Err(error) => {
            let settled = error.cleanup_verified();
            let cause = std::io::Error::other(error.to_string());
            if !settled {
                latch_bounded_unknown(true);
                quarantine_bounded_owner(Box::new((error, owner)));
            }
            return Err(std::io::Error::other(BoundedExecutionError {
                cause,
                settlement: if settled {
                    BoundedSettlement::Proven
                } else {
                    BoundedSettlement::Unknown
                },
            }));
        }
    };
    output_bounded_spawned(
        child,
        containment,
        input,
        timeout,
        drain_grace,
        owner,
        strict,
        None,
    )
    .map_err(std::io::Error::other)
}

pub(crate) fn configure_bounded_stdio(cmd: &mut Command, input: bool) {
    use std::process::Stdio;
    cmd.stdin(if input { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

struct BoundedBody {
    child: std::sync::Arc<std::sync::Mutex<Child>>,
    containment: std::sync::Arc<Containment>,
    lifetime: std::sync::Arc<BoundedLifetime>,
    strict_unknown: bool,
    completed: bool,
}
impl BoundedBody {
    fn terminate(&self) {
        terminate_tree(&self.containment);
        // Never call wait() here: pipe/child cleanup is bounded independently of the observer.
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
    fn error(&self, cause: std::io::Error) -> BoundedExecutionError {
        latch_bounded_unknown(self.strict_unknown);
        BoundedExecutionError {
            cause,
            settlement: BoundedSettlement::Unknown,
        }
    }
    fn startup_error(&mut self, cause: std::io::Error, grace: Duration) -> BoundedExecutionError {
        #[cfg(test)]
        BOUNDED_STARTUP_FAILURE_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook(self);
            }
        });
        // Thread::spawn drops the failed worker's closure/token. Keep the actual child and caller
        // owner here until a bounded reap; kill alone would leave a Unix zombie and let a legacy
        // lifetime's last token release its guard/context before the child has actually settled.
        self.terminate();
        let deadline = std::time::Instant::now()
            .checked_add(grace)
            .unwrap_or_else(std::time::Instant::now);
        let reaped = loop {
            let status = self
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_wait();
            match status {
                Ok(Some(_)) => break true,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(
                        Duration::from_millis(10)
                            .min(deadline.saturating_duration_since(std::time::Instant::now())),
                    );
                }
                _ => break false,
            }
        };
        if !reaped {
            // No new reaper thread is reliable after thread-start failure. Fail closed, retaining
            // the existing lifetime (child, containment, caller/guard and any actual workers) even
            // for legacy callers. Never turn a bounded observer into an unbounded Child::wait.
            latch_bounded_unknown(true);
            quarantine_bounded_owner(Box::new(self.lifetime.clone()));
        }
        // Reaped legacy children still need every actual worker token to finish. Strict UNKNOWN
        // continues to use the lifetime's existing quarantine; direct exit is not a tree proof.
        self.completed = true;
        self.error(cause)
    }
}
impl Drop for BoundedBody {
    fn drop(&mut self) {
        if !self.completed {
            self.terminate();
            latch_bounded_unknown(self.strict_unknown);
        }
    }
}

struct BoundedDrain {
    bytes: Vec<u8>,
    truncated: bool,
}

#[cfg(test)]
type BoundedTestDrainRelease = std::sync::Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>;
#[cfg(test)]
type BoundedStartupFailureHook = Box<dyn Fn(&BoundedBody)>;

#[cfg(test)]
thread_local! {
    // Countdown is local to the spawning test thread; unrelated suite workers cannot consume it.
    static FAIL_BOUNDED_WORKER_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static BOUNDED_STARTUP_FAILURE_HOOK: std::cell::RefCell<Option<BoundedStartupFailureHook>> = const { std::cell::RefCell::new(None) };
    static BOUNDED_TEST_DRAIN_RELEASE: std::cell::RefCell<std::collections::VecDeque<BoundedTestDrainRelease>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

fn spawn_bounded_worker<T: Send + 'static>(
    name: &str,
    worker: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<T>> {
    #[cfg(test)]
    if FAIL_BOUNDED_WORKER_AFTER.with(|value| match value.get() {
        Some(0) => {
            value.set(None);
            true
        }
        Some(remaining) => {
            value.set(Some(remaining - 1));
            false
        }
        None => false,
    }) {
        // Match Builder::spawn failure: the unstarted worker, pipe and token are dropped here.
        return Err(std::io::Error::other(
            "injected bounded worker thread-start failure",
        ));
    }
    std::thread::Builder::new().name(name.into()).spawn(worker)
}

fn spawn_bounded_drain<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
    token: crate::agent::process::DrainLifetime<BoundedOwnership>,
    cap: Option<usize>,
) -> std::io::Result<std::sync::mpsc::Receiver<BoundedDrain>> {
    let (tx, rx) = std::sync::mpsc::channel();
    #[cfg(test)]
    let release = BOUNDED_TEST_DRAIN_RELEASE.with(|value| value.borrow_mut().pop_front());
    spawn_bounded_worker("bounded-pipe", move || {
        let result = {
            let mut bytes = Vec::new();
            let mut truncated = false;
            if let Some(mut pipe) = pipe {
                let mut buffer = [0u8; 8192];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let keep =
                                cap.map_or(count, |cap| cap.saturating_sub(bytes.len()).min(count));
                            bytes.extend_from_slice(&buffer[..keep]);
                            truncated |= keep != count;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            truncated = true;
                            break;
                        }
                    }
                }
            }
            BoundedDrain { bytes, truncated }
        };
        // Publication follows actual reader completion and lifetime-token release, never vice versa.
        #[cfg(test)]
        if let Some(release) = release.as_ref() {
            let released = release.0.lock().unwrap_or_else(|e| e.into_inner());
            // A real started reader keeps its token; even failed assertions cannot strand the test
            // worker forever. Only the spawning thread copies this test-local latch into its worker.
            let _ = release
                .1
                .wait_timeout_while(released, Duration::from_secs(10), |v| !v.0);
        }
        drop(token);
        #[cfg(test)]
        if let Some(release) = release {
            release.0.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
            release.1.notify_all();
        }
        let _ = tx.send(result);
    })?;
    Ok(rx)
}

/// Shared actual-body helper. All worker tokens are registered before any tree proof. An observer
/// deadline never releases owners held by active drains; strict UNKNOWN quarantines the whole body.
#[allow(clippy::too_many_arguments)]
pub(crate) fn output_bounded_spawned(
    mut child: Child,
    containment: Containment,
    input: Option<Vec<u8>>,
    timeout: Duration,
    drain_grace: Duration,
    owner: Option<BoundedOwner>,
    strict_unknown: bool,
    cap: Option<usize>,
) -> Result<BoundedBytes, BoundedExecutionError> {
    use std::sync::{Arc, Mutex};
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let stdin = child.stdin.take();
    let child = Arc::new(Mutex::new(child));
    let containment = Arc::new(containment);
    let lifetime = Arc::new(BoundedLifetime::new(
        BoundedOwnership {
            _child: child.clone(),
            _containment: containment.clone(),
            _caller: owner,
            strict_unknown,
        },
        quarantine_bounded_body,
    ));
    let mut body = BoundedBody {
        child,
        containment,
        lifetime,
        strict_unknown,
        completed: false,
    };
    let out_token = body.lifetime.drain();
    let err_token = body.lifetime.drain();
    let input_token = body.lifetime.drain();
    let out = spawn_bounded_drain(out_pipe, out_token, cap)
        .map_err(|e| body.startup_error(e, drain_grace))?;
    let err = spawn_bounded_drain(err_pipe, err_token, cap)
        .map_err(|e| body.startup_error(e, drain_grace))?;
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    if let (Some(bytes), Some(mut stdin)) = (input, stdin) {
        spawn_bounded_worker("bounded-stdin", move || {
            use std::io::Write;
            let _ = stdin.write_all(&bytes);
            drop(stdin);
            drop(input_token);
            let _ = input_tx.send(());
        })
        .map_err(|e| body.startup_error(e, drain_grace))?;
    } else {
        drop(input_token);
        let _ = input_tx.send(());
    }
    let started = std::time::Instant::now();
    let mut timed_out = false;
    let mut cleanup_started = None;
    let code = loop {
        let status = body
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_wait();
        match status {
            Ok(Some(status)) => break if timed_out { None } else { status.code() },
            Ok(None) => {
                if !timed_out && started.elapsed() >= timeout {
                    timed_out = true;
                    body.terminate();
                    cleanup_started = Some(std::time::Instant::now());
                }
                if cleanup_started.is_some_and(|start| start.elapsed() >= drain_grace) {
                    return Err(body.error(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "owned child did not exit during bounded cleanup",
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(body.error(error)),
        }
    };
    let deadline = std::time::Instant::now()
        .checked_add(drain_grace)
        .unwrap_or_else(std::time::Instant::now);
    let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
    let stdout = out.recv_timeout(remaining()).ok();
    let stderr = err.recv_timeout(remaining()).ok();
    let input_done = input_rx.recv_timeout(remaining()).is_ok();
    let all_workers_done = stdout.is_some() && stderr.is_some() && input_done;
    let tree_settled = loop {
        match body.containment.known_quiescent() {
            Ok(Some(true)) => break true,
            Ok(Some(false)) if std::time::Instant::now() < deadline => std::thread::yield_now(),
            _ => break false,
        }
    };
    let settlement = if tree_settled && all_workers_done {
        body.lifetime.prove_tree_settled();
        BoundedSettlement::Proven
    } else {
        // Kill only the owned tree on incomplete settlement; no unbounded wait/reap here.
        if strict_unknown || !all_workers_done {
            body.terminate();
        }
        latch_bounded_unknown(strict_unknown);
        BoundedSettlement::Unknown
    };
    let output_truncated =
        stdout.as_ref().is_none_or(|v| v.truncated) || stderr.as_ref().is_none_or(|v| v.truncated);
    let stdout = stdout.map(|v| v.bytes).unwrap_or_default();
    let stderr = stderr.map(|v| v.bytes).unwrap_or_default();
    body.completed = true;
    Ok(BoundedBytes {
        stdout,
        stderr,
        code,
        timed_out,
        output_truncated,
        settlement,
    })
}

/// [`prepare`] for the async spawn path.
pub fn prepare_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // SAFETY: as `prepare` — `setsid` is async-signal-safe and touches no allocator or lock.
        // A failed `setsid` fails the spawn rather than leaving an uncontained child that would
        // later read as settled.
        unsafe {
            cmd.pre_exec(libc_setsid);
        }
    }
    #[cfg(windows)]
    {
        // tokio's Command exposes `creation_flags` inherently on Windows (no trait import). See
        // `CREATE_NO_WINDOW` for why every spawned child gets it.
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

/// Legacy group attachment used only by the non-Windows protected spawn fallback.
#[cfg(not(windows))]
pub fn contain_tokio(child: &tokio::process::Child) -> Containment {
    #[cfg(unix)]
    {
        // `prepare_tokio` made the child a session leader ⇒ its pid is the group id. A child already
        // reaped has no id; nothing to contain then.
        return match child.id() {
            Some(pid) => Containment::Group(pid as i32),
            None => Containment::None,
        };
    }
    #[allow(unreachable_code)]
    {
        let _ = child;
        Containment::None
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "setsid"]
    fn c_setsid() -> i32;
    #[link_name = "killpg"]
    fn c_killpg(pgrp: i32, sig: i32) -> i32;
}

/// `ESRCH` is 3 on both Linux and Darwin. A process group we created with `setsid` reports it once
/// its last member is gone; `EPERM` (or any other failure) means "may still exist" and keeps
/// ownership charged.
#[cfg(unix)]
const ESRCH: i32 = 3;

/// Is the process group we created empty? Signal 0 performs only the existence/permission check.
#[cfg(unix)]
fn group_is_empty(pgid: i32) -> bool {
    // SAFETY: signal 0 sends nothing; the only effect is the kernel's group lookup.
    let result = unsafe { c_killpg(pgid, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(ESRCH)
}

// Thin wrappers so the `unsafe` call sites above read as one operation each. Declared rather than
// pulled from `libc`: these two symbols are all we need, and the crate is not in the dependency
// tree (single static binary posture — see Cargo.toml).
#[cfg(unix)]
fn libc_setsid() -> std::io::Result<()> {
    // SAFETY: no arguments, no memory touched.
    if unsafe { c_setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
unsafe fn libc_killpg(pgrp: i32, sig: i32) {
    c_killpg(pgrp, sig);
}

#[cfg(windows)]
pub mod windows_job {
    //! Kill-on-close Job Object for a `std::process::Child`.
    //!
    //! This mirrors `agent::lsp::jobobject` (which contains *tokio* children for language servers).
    //! The duplication is deliberate and small: that module is typed against `tokio::process::Child`
    //! and lives behind the LSP feature surface, while shell/verify children are `std::process`.
    //! Unifying them would mean a generic over raw-handle extraction for a 30-line binding.

    use super::Containment;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Resource ceilings a job may carry on top of kill-on-close. All optional: `default()` is
    /// exactly the old behavior. Set by the sandbox runner from the resolved policy.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct JobLimits {
        /// Max simultaneously-live processes in the job (fork-bomb stop).
        pub active_process_limit: Option<u32>,
        /// Committed-memory ceiling for the WHOLE job, bytes.
        pub job_memory_bytes: Option<u64>,
        /// User-mode CPU-time ceiling for the whole job; the job is terminated when exceeded.
        pub job_user_time: Option<std::time::Duration>,
    }

    impl JobLimits {
        fn is_default(&self) -> bool {
            self.active_process_limit.is_none()
                && self.job_memory_bytes.is_none()
                && self.job_user_time.is_none()
        }
    }

    /// An owned kill-on-close job handle. Only the private suspended-spawn path can certify that
    /// assignment preceded execution; ordinary late attachment cannot prove complete descendants.
    pub struct Job {
        handle: HANDLE,
        protected_before_execution: bool,
    }

    // SAFETY: a job handle is a kernel object usable from any thread; we only ever close it once
    // (in Drop) and never hand out the raw value.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        fn kill_on_close() -> Option<Self> {
            Self::kill_on_close_with(&JobLimits::default())
        }

        fn kill_on_close_with(limits: &JobLimits) -> Option<Self> {
            // SAFETY: null attributes + null name are the documented "anonymous job" arguments; the
            // handle is checked before use and owned by `Job` (closed exactly once in Drop).
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let job = Job {
                handle,
                protected_before_execution: false,
            };
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(n) = limits.active_process_limit {
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                info.BasicLimitInformation.ActiveProcessLimit = n;
            }
            if let Some(bytes) = limits.job_memory_bytes {
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                info.JobMemoryLimit = bytes as usize;
            }
            if let Some(t) = limits.job_user_time {
                // 100-nanosecond units, per the JOBOBJECT_BASIC_LIMIT_INFORMATION contract.
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_TIME;
                info.BasicLimitInformation.PerJobUserTimeLimit = (t.as_nanos() / 100) as i64;
            }
            // SAFETY: `info` is initialized and the size argument is exactly its size — the
            // (pointer, length) pair the API requires.
            let ok = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            (ok != 0).then_some(job)
        }

        pub(super) fn known_quiescent(&self) -> std::io::Result<Option<bool>> {
            if self.protected_before_execution {
                self.active_processes().map(|count| Some(count == 0))
            } else {
                Ok(None)
            }
        }

        /// Read-only job membership count, not a request to terminate anything.
        pub(crate) fn active_processes(&self) -> std::io::Result<u32> {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicAccountingInformation,
                    std::ptr::from_mut(&mut info).cast(),
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(info.ActiveProcesses)
            }
        }

        /// Terminate every process in the job now (rather than waiting for the handle to close).
        pub fn terminate(&self) {
            // SAFETY: `self.handle` is a live handle we own. Exit code 1 marks an abnormal end.
            unsafe { TerminateJobObject(self.handle, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: single close of a handle we own. Kill-on-close fires here, so any tree still
            // running when the caller drops containment is reaped.
            unsafe { CloseHandle(self.handle) };
        }
    }

    /// Assign a freshly-spawned child (and thus its descendants) to a new kill-on-close job.
    pub fn contain(child: &std::process::Child) -> Containment {
        assign(child.as_raw_handle() as HANDLE)
    }

    /// [`contain`], with resource ceilings from the sandbox policy on the same job.
    pub fn contain_with_limits(child: &std::process::Child, limits: &JobLimits) -> Containment {
        assign_with(child.as_raw_handle() as HANDLE, limits)
    }

    #[cfg(test)]
    thread_local! {
        static FAIL_PROTECTED_RESUME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[cfg(test)]
    pub(crate) fn with_test_resume_failure<T>(f: impl FnOnce() -> T) -> T {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                FAIL_PROTECTED_RESUME.with(|value| value.set(self.0));
            }
        }
        let previous = FAIL_PROTECTED_RESUME.with(|value| value.replace(true));
        let _restore = Restore(previous);
        f()
    }

    /// Private failure carrier: only an OWNED CREATE_SUSPENDED spawn may enter this path.
    pub(super) struct ProtectionFailure {
        pub(super) cause: std::io::Error,
        pub(super) cleanup_verified: bool,
        pub(super) containment: Containment,
    }

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Called only by the owned suspended-spawn functions, never by ordinary late attach APIs.
    pub(super) fn protect_suspended(
        process: HANDLE,
        pid: u32,
        limits: Option<&JobLimits>,
    ) -> Result<Containment, ProtectionFailure> {
        use windows_sys::Win32::System::Threading::{
            ResumeThread, TerminateProcess, WaitForSingleObject,
        };
        let mut containment = Containment::None;
        let result = (|| -> std::io::Result<()> {
            // Protected admission does NOT silently discard configured resource ceilings.
            let job = Job::kill_on_close_with(limits.unwrap_or(&JobLimits::default()))
                .ok_or_else(std::io::Error::last_os_error)?;
            if unsafe { AssignProcessToJobObject(job.handle, process) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            containment = Containment::Job(job);
            let initial = initial_thread(pid)?;
            #[cfg(test)]
            if FAIL_PROTECTED_RESUME.with(std::cell::Cell::get) {
                return Err(std::io::Error::other("injected protected resume refusal"));
            }
            // CREATE_SUSPENDED contributes exactly one suspension. A zero count means the child
            // already executed; >1 means it remains suspended. Neither may certify protected spawn.
            let previous = unsafe { ResumeThread(initial.0) };
            if previous == u32::MAX {
                return Err(std::io::Error::last_os_error());
            }
            if previous != 1 {
                return Err(std::io::Error::other(format!(
                    "initial thread suspend count was {previous}, expected one"
                )));
            }
            if let Containment::Job(job) = &mut containment {
                job.protected_before_execution = true;
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(containment),
            Err(cause) => {
                // Failure cleanup targets only the child JUST CREATED by our private suspended path.
                // If resume unexpectedly ran it, its assigned job still excludes breakaway children.
                let job_termination = match &containment {
                    Containment::Job(job) => unsafe { TerminateJobObject(job.handle, 1) != 0 },
                    _ => true,
                };
                let process_termination = unsafe { TerminateProcess(process, 1) != 0 };
                let wait = unsafe { WaitForSingleObject(process, 2_000) };
                let job_empty = match &containment {
                    Containment::Job(job) => job.active_processes().is_ok_and(|count| count == 0),
                    _ => true,
                };
                // WAIT_OBJECT_0 is zero. An already-exited process may legitimately refuse terminate;
                // the wait result and empty job, not API intention, are the cleanup proof.
                let cleanup_verified = wait == 0 && job_empty;
                let cause = std::io::Error::other(format!(
                    "{cause}; owned cleanup: job terminate={job_termination}, process terminate={process_termination}, wait={wait}, job empty={job_empty}"
                ));
                Err(ProtectionFailure {
                    cause,
                    cleanup_verified,
                    containment,
                })
            }
        }
    }

    fn initial_thread(pid: u32) -> std::io::Result<OwnedHandle> {
        use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{
            GetProcessIdOfThread, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
            THREAD_SUSPEND_RESUME,
        };
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let snapshot = OwnedHandle(raw);
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if unsafe { Thread32First(snapshot.0, &mut entry) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut found = None;
        let mut count = 0usize;
        loop {
            count += 1;
            if count > 65_536 {
                return Err(std::io::Error::other(
                    "thread snapshot exceeds protected spawn bound",
                ));
            }
            if entry.th32OwnerProcessID == pid && found.replace(entry.th32ThreadID).is_some() {
                return Err(std::io::Error::other(
                    "suspended child has ambiguous initial threads",
                ));
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            if unsafe { Thread32Next(snapshot.0, &mut entry) } == 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                    return Err(error);
                }
                break;
            }
        }
        let tid = found
            .ok_or_else(|| std::io::Error::other("suspended child's initial thread not found"))?;
        let raw = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                0,
                tid,
            )
        };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let thread = OwnedHandle(raw);
        if unsafe { GetProcessIdOfThread(thread.0) } != pid {
            return Err(std::io::Error::other(
                "suspended child thread identity changed",
            ));
        }
        Ok(thread)
    }

    fn assign(process: HANDLE) -> Containment {
        assign_with(process, &JobLimits::default())
    }

    fn assign_with(process: HANDLE, limits: &JobLimits) -> Containment {
        let job = if limits.is_default() {
            Job::kill_on_close()
        } else {
            // A job that refuses the limits still gets kill-on-close: containment must never be
            // LOST because a ceiling could not be set (the ceiling is the add-on, not the core).
            Job::kill_on_close_with(limits).or_else(Job::kill_on_close)
        };
        let Some(job) = job else {
            return Containment::None;
        };
        // SAFETY: both handles are live — the job is owned locally, the process handle belongs to a
        // child the caller still owns.
        let ok = unsafe { AssignProcessToJobObject(job.handle, process) != 0 };
        if ok {
            Containment::Job(job)
        } else {
            Containment::None
        }
    }

    #[cfg(test)]
    mod proof_tests {
        use super::*;

        #[test]
        fn ordinary_late_job_attachment_never_certifies_descendants() {
            let job = Job::kill_on_close().expect("create empty test-owned job");
            assert_eq!(job.active_processes().unwrap(), 0);
            assert_eq!(job.known_quiescent().unwrap(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    struct StartupFailureInjection {
        previous_failure: Option<usize>,
        previous_hook: Option<BoundedStartupFailureHook>,
        previous_release: std::collections::VecDeque<BoundedTestDrainRelease>,
    }
    impl StartupFailureInjection {
        fn new(
            after: usize,
            hook: impl Fn(&BoundedBody) + 'static,
            release: std::collections::VecDeque<BoundedTestDrainRelease>,
        ) -> Self {
            Self {
                previous_failure: FAIL_BOUNDED_WORKER_AFTER.with(|v| v.replace(Some(after))),
                previous_hook: BOUNDED_STARTUP_FAILURE_HOOK
                    .with(|v| v.replace(Some(Box::new(hook)))),
                previous_release: BOUNDED_TEST_DRAIN_RELEASE.with(|v| v.replace(release)),
            }
        }
    }
    impl Drop for StartupFailureInjection {
        fn drop(&mut self) {
            FAIL_BOUNDED_WORKER_AFTER.with(|v| v.set(self.previous_failure));
            BOUNDED_STARTUP_FAILURE_HOOK.with(|v| {
                v.replace(self.previous_hook.take());
            });
            BOUNDED_TEST_DRAIN_RELEASE.with(|v| {
                v.replace(std::mem::take(&mut self.previous_release));
            });
        }
    }

    #[test]
    fn bounded_worker_failure_injection_is_thread_local() {
        let _injection = StartupFailureInjection::new(0, |_| {}, Default::default());
        std::thread::spawn(|| {
            spawn_bounded_worker("bounded-test-unrelated", || ())
                .expect("another suite thread must not consume injection")
                .join()
                .unwrap();
        })
        .join()
        .unwrap();
        assert!(spawn_bounded_worker("bounded-test-injected", || ()).is_err());
        spawn_bounded_worker("bounded-test-after-injection", || ())
            .expect("failure injection is one-shot")
            .join()
            .unwrap();
    }

    #[test]
    fn bounded_startup_fixture() -> anyhow::Result<()> {
        if !std::env::args().any(|arg| arg == "core::proctree::tests::bounded_startup_fixture") {
            return Ok(());
        }
        let Some(root) = std::env::var_os("AIZEN_BOUNDED_STARTUP_FIXTURE_ROOT") else {
            return Ok(());
        };
        let root = std::fs::canonicalize(root)?;
        let temp = std::fs::canonicalize(std::env::temp_dir())?;
        anyhow::ensure!(
            root.starts_with(&temp) && root != temp,
            "fixture must be test-owned"
        );
        // Only this sandbox-prepared test helper is killed by startup cleanup, never a user process.
        std::thread::sleep(Duration::from_secs(30));
        Ok(())
    }

    fn assert_bounded_startup_failure(after: usize) -> anyhow::Result<()> {
        use crate::sandbox::request::SandboxRequest;
        use crate::sandbox::{runner, CommandOrigin};
        use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
        type SeenBody = (Weak<Mutex<Child>>, Weak<Containment>);
        struct Owner {
            pid: u32,
            released: mpsc::Sender<bool>,
            _guard: Option<Arc<Mutex<runner::SpawnGuard>>>,
            _context: Option<crate::core::exec_ctx::ExecutionContext>,
        }
        impl Drop for Owner {
            fn drop(&mut self) {
                #[cfg(unix)]
                let reaped = {
                    extern "C" {
                        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
                    }
                    // SAFETY: positive pid names only this test's just-spawned owned fixture.
                    // WNOHANG never waits. -1/ECHILD proves cleanup already reaped it; a remaining
                    // zombie would return the pid (and be reaped by this failed assertion's probe).
                    let result = unsafe { waitpid(self.pid as i32, std::ptr::null_mut(), 1) };
                    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(10)
                };
                #[cfg(not(unix))]
                let reaped = {
                    let _ = self.pid;
                    true // Unix waitpid/ECHILD assertion has no Windows equivalent.
                };
                drop(self._guard.take());
                drop(self._context.take());
                let _ = self.released.send(reaped);
            }
        }
        struct ReleaseReaders(BoundedTestDrainRelease);
        impl Drop for ReleaseReaders {
            fn drop(&mut self) {
                self.0 .0.lock().unwrap_or_else(|e| e.into_inner()).0 = true;
                self.0 .1.notify_all();
            }
        }

        let _home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let home = fixture.path().join("home");
        std::fs::create_dir(&home)?;
        let _env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        let request = SandboxRequest::exec(
            CommandOrigin::InternalTrusted("bounded startup failure fixture"),
            std::env::current_exe()?,
            vec![
                "--exact".into(),
                "core::proctree::tests::bounded_startup_fixture".into(),
                "--nocapture".into(),
            ],
            fixture.path().to_path_buf(),
            fixture.path().to_path_buf(),
        )
        .private_tmp(false)
        .background(false)
        .extra_env(vec![
            ("AIZEN_HOME".into(), home.to_str().unwrap().into()),
            (
                "AIZEN_BOUNDED_STARTUP_FIXTURE_ROOT".into(),
                fixture.path().to_str().unwrap().into(),
            ),
        ]);
        let mut prepared = runner::prepare_std(request)?;
        configure_bounded_stdio(&mut prepared.command, after == 2);
        let (child, containment) = prepared.spawn_contained()?;
        let guard = Arc::new(Mutex::new(prepared.into_guard()));
        let weak_guard = Arc::downgrade(&guard);
        let (released, observed) = mpsc::channel();
        let owner = Owner {
            pid: child.id(),
            released,
            _guard: Some(guard),
            _context: Some(crate::core::exec_ctx::ExecutionContext::new(
                crate::core::convo::ConversationId::new("bounded-startup-fixture"),
            )),
        };
        let seen: Arc<Mutex<Option<SeenBody>>> = Arc::new(Mutex::new(None));
        let seen_hook = seen.clone();
        let releases: Vec<_> = (0..after)
            .map(|_| Arc::new((Mutex::new((false, false)), Condvar::new())))
            .collect();
        let mut release_readers: std::collections::VecDeque<_> =
            releases.iter().cloned().map(ReleaseReaders).collect();
        let _injection = StartupFailureInjection::new(
            after,
            move |body| {
                *seen_hook.lock().unwrap() = Some((
                    Arc::downgrade(&body.child),
                    Arc::downgrade(&body.containment),
                ));
            },
            releases.iter().cloned().collect(),
        );
        let started = std::time::Instant::now();
        let result = output_bounded_spawned(
            child,
            containment,
            (after == 2).then(|| vec![0; 32]),
            Duration::from_secs(30),
            Duration::from_secs(2),
            Some(Box::new(owner)),
            false,
            None,
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => anyhow::bail!("injected startup failure unexpectedly succeeded"),
        };
        assert!(error.cause.to_string().contains("injected bounded worker"));
        assert_eq!(error.settlement, BoundedSettlement::Unknown);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cleanup exceeded grace cap"
        );
        let (child, containment) = seen.lock().unwrap().clone().expect("startup cleanup hook");
        if after != 0 {
            // These are REAL stdout (and, for stdin failure, stderr) worker tokens, kept alive by
            // the test-local release latch even after EOF. Observer/error return cannot free them.
            assert!(matches!(
                observed.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(
                weak_guard.upgrade().is_some(),
                "sandbox guard dropped before actual drains"
            );
            let child = child
                .upgrade()
                .expect("actual child retained by started drain");
            assert!(
                child.lock().unwrap().try_wait()?.is_some(),
                "startup child not reaped"
            );
            assert!(
                containment.upgrade().is_some(),
                "containment dropped before actual drains"
            );
        }
        for (index, release) in releases.iter().enumerate() {
            drop(release_readers.pop_front());
            let state = release.0.lock().unwrap();
            let (state, _) = release
                .1
                .wait_timeout_while(state, Duration::from_secs(2), |v| !v.1)
                .unwrap();
            assert!(state.1, "released drain did not finish its actual token");
            if index + 1 < after {
                assert!(matches!(
                    observed.try_recv(),
                    Err(mpsc::TryRecvError::Empty)
                ));
                assert!(
                    weak_guard.upgrade().is_some(),
                    "another active drain lost its owner"
                );
            }
        }
        assert!(
            observed.recv_timeout(Duration::from_secs(2))?,
            "owner released before Unix reap"
        );
        assert!(
            weak_guard.upgrade().is_none(),
            "settled legacy guard still retained"
        );
        Ok(())
    }

    #[test]
    fn bounded_stdout_start_failure_reaps_before_owner_release() -> anyhow::Result<()> {
        assert_bounded_startup_failure(0)
    }

    #[test]
    fn bounded_stderr_start_failure_retains_started_stdout_owner() -> anyhow::Result<()> {
        assert_bounded_startup_failure(1)
    }

    #[test]
    fn bounded_stdin_start_failure_retains_both_started_drain_owners() -> anyhow::Result<()> {
        assert_bounded_startup_failure(2)
    }

    #[test]
    fn bounded_owner_waits_for_actual_worker_and_unknown_quarantine() {
        use std::sync::{mpsc, Arc};
        struct Owner(mpsc::Sender<()>);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        fn unexpected_quarantine(_: Owner) {
            panic!("proven owner must not quarantine");
        }
        let (released, observed) = mpsc::channel();
        let lifetime = Arc::new(crate::agent::process::SettlementLifetime::new(
            Owner(released),
            unexpected_quarantine,
        ));
        let drain = lifetime.drain();
        lifetime.prove_tree_settled();
        drop(lifetime); // Observer exit cannot release the actual worker's owner.
        assert!(matches!(
            observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(drain);
        observed
            .recv_timeout(Duration::from_secs(1))
            .expect("actual drain releases proven owner");

        struct Unknown {
            released: mpsc::Sender<()>,
            quarantined: mpsc::Sender<()>,
        }
        impl Drop for Unknown {
            fn drop(&mut self) {
                let _ = self.released.send(());
            }
        }
        fn quarantine(owner: Unknown) {
            owner.quarantined.send(()).unwrap();
            // Test-only owner; no OS lock, process or global unknown latch.
            std::mem::forget(owner);
        }
        let (released, observed) = mpsc::channel();
        let (quarantined, seen) = mpsc::channel();
        let lifetime = Arc::new(crate::agent::process::SettlementLifetime::new(
            Unknown {
                released,
                quarantined,
            },
            quarantine,
        ));
        let drain = lifetime.drain();
        drop(lifetime);
        assert!(seen.try_recv().is_err());
        drop(drain);
        seen.recv_timeout(Duration::from_secs(1))
            .expect("unknown owner quarantined after actual worker");
        assert!(matches!(
            observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn missing_containment_never_proves_physical_settlement() {
        assert_eq!(Containment::None.known_quiescent().unwrap(), None);
        // A group id we could not have created must never be signalled.
        #[cfg(unix)]
        assert_eq!(Containment::Group(0).known_quiescent().unwrap(), None);
    }

    /// The Unix counterpart of the Job Object proof: settlement is reported only once the group we
    /// created is empty, never on wrapper exit alone. Before this, every Unix strict body reported
    /// UNKNOWN forever, so hooks discarded their verdicts and process fixtures aborted at teardown.
    #[cfg(unix)]
    #[test]
    fn unix_process_group_proves_settlement_only_after_its_members_exit() {
        let mut cmd = shell("sleep 5");
        prepare(&mut cmd);
        let mut child = cmd.spawn().expect("spawn shell");
        let containment = contain(&child);
        assert!(
            matches!(containment.known_quiescent().unwrap(), Some(false)),
            "a live group must not read as settled: {containment:?}"
        );
        kill_tree(&mut child, &containment);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while containment.known_quiescent().unwrap() != Some(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "killed group never settled: {containment:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The shell wrapper Aizen actually uses, so the test exercises the real spawn shape.
    fn shell(command: &str) -> Command {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    #[test]
    fn containment_is_available_on_this_platform() {
        // Not a tautology: on Windows this asserts the Job Object APIs actually work here, which is
        // the whole basis of tree-kill. A platform without them degrades to killing the direct
        // child, so the assertion is scoped to the platforms we claim support for.
        let mut cmd = shell("echo hi");
        prepare(&mut cmd);
        let mut child = cmd.spawn().expect("spawn shell");
        let c = contain(&child);
        assert!(
            c.is_contained(),
            "job/group containment must be available: {c:?}"
        );
        kill_tree(&mut child, &c);
    }

    /// A long-lived grandchild that keeps the inherited pipe open after the wrapper is killed.
    ///
    /// The choice is empirical, not stylistic. `cmd /C ping -n 40 …` — the obvious portable
    /// busy-wait — turned out to DIE with its wrapper on Windows 11 26200, so a test built on it
    /// passed whether or not tree-kill worked. A `powershell Start-Sleep` grandchild was measured
    /// surviving the wrapper kill *and* holding the pipe (reader blocked), which is the state this
    /// module exists to escape. Absolute path: PATH is not guaranteed to carry System32.
    fn sleeper_command() -> String {
        if cfg!(windows) {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            format!(
                r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -Command Start-Sleep -Seconds 40"
            )
        } else {
            // Background + wait, so `sh` does NOT exec-replace itself with `sleep`: we need a real
            // grandchild, otherwise killing the direct child would already end everything.
            "sleep 40 & wait".to_string()
        }
    }

    /// Read to EOF on a thread, reporting whether it finished within `wait`.
    fn reader_finished_within(
        rx: &std::sync::mpsc::Receiver<usize>,
        wait: std::time::Duration,
    ) -> bool {
        rx.recv_timeout(wait).is_ok()
    }

    #[test]
    fn kill_tree_closes_the_pipes_so_a_reader_sees_eof() {
        // The regression this module exists for: a grandchild outliving the `cmd.exe` wrapper holds
        // the inherited write end, so `read_to_end` never sees EOF and the drain thread blocks
        // forever.
        //
        // The test carries its own NEGATIVE CONTROL, because an earlier version of it silently
        // proved nothing: first we kill ONLY the direct child and require the reader to still be
        // blocked (⇒ a descendant really does hold the pipe — the precondition), and only then do we
        // terminate the tree and require the reader to complete. If containment ever regresses to
        // "kill the wrapper", the second half fails; if the fixture stops reproducing the hang, the
        // first half fails instead of quietly passing.
        use std::io::Read;
        let mut cmd = shell(&sleeper_command());
        prepare(&mut cmd);
        let mut child = cmd.spawn().expect("spawn shell");
        let containment = contain(&child);
        let mut out = child.stdout.take().expect("piped stdout");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            let _ = tx.send(buf.len());
        });

        // Let the grandchild come up and inherit the pipe.
        std::thread::sleep(std::time::Duration::from_millis(900));

        // Kill the direct child ONLY — the old behaviour.
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !reader_finished_within(&rx, std::time::Duration::from_millis(1500)),
            "fixture no longer reproduces the hang: the pipe closed after killing just the wrapper, \
             so the tree-kill assertion below would prove nothing. Fix the fixture, not the assert."
        );

        // Now the real thing.
        terminate_tree(&containment);
        assert!(
            reader_finished_within(&rx, std::time::Duration::from_secs(15)),
            "pipe reader still blocked after tree-kill — a descendant kept the write end open"
        );
    }

    /// `output_bounded_bytes` must hand back stdout VERBATIM.
    ///
    /// The Time Machine reads file contents through `git cat-file blob`, so a lossy UTF-8 decode
    /// anywhere on that path silently rewrites every invalid byte to U+FFFD — corrupting any
    /// non-UTF-8 file a checkpoint captures, diffs, or restores. This pins the guarantee that made
    /// it safe to route the Time Machine's git calls through the bounded runner at all.
    #[test]
    fn bounded_bytes_preserves_invalid_utf8() {
        // 0xFF is never valid UTF-8. Getting a child to emit one, portably, is fiddlier than it
        // looks, and two earlier fixtures failed for reasons worth recording so nobody re-tries them:
        // a PowerShell `[Console]::OpenStandardOutput().Write(…)` one-liner is refused outright by
        // AMSI ("script contains malicious content"), and `cmd /C type "<path>"` mis-parses the
        // quotes Rust adds around an argument containing them.
        //
        // So: write the byte to a file, then read it back through the platform's cat with the
        // child's WORKING DIRECTORY set and the bare filename as its own argument. No path — and
        // therefore no quoting — ever reaches the command line, and the generated filename has no
        // spaces. `dir`/`file` are captured by value so each call gets an independent Command.
        let dir = std::env::temp_dir();
        let name = format!(
            "aizen-proctree-bytes-{}-{}.bin",
            std::process::id(),
            crate::core::persist::unique_sequence()
        );
        std::fs::write(dir.join(&name), [0x41u8, 0xFF, 0x42]).expect("write fixture");
        let emit = || -> Command {
            let mut c = if cfg!(windows) {
                // `type` is a cmd builtin, so cmd is unavoidable here — but with cwd set, the only
                // arguments are the literal words `type` and the filename.
                let mut c = Command::new("cmd");
                c.arg("/C").arg("type").arg(&name);
                c
            } else {
                let mut c = Command::new("cat");
                c.arg(&name);
                c
            };
            c.current_dir(&dir);
            c
        };
        let mut cmd = emit();
        let out = output_bounded_bytes(
            &mut cmd,
            std::time::Duration::from_secs(20),
            std::time::Duration::from_secs(2),
        )
        .expect("bounded run");
        assert!(!out.timed_out, "trivial command must not hit the deadline");
        // Precondition: the fixture actually produced output. Without this, an unlaunchable shell
        // makes the byte assertion below fail for the wrong reason.
        assert!(
            !out.stdout.is_empty(),
            "fixture produced no output (code={:?}, stderr={:?}) — the shell never ran, so this \
             test proves nothing about byte fidelity",
            out.code,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stdout.contains(&0xFF),
            "raw 0xFF byte was not preserved (stdout={:?}) — a lossy decode crept back in",
            out.stdout
        );
        // And the lossy wrapper must still be lossy, proving the two shapes are genuinely distinct
        // — same fixture, so the only difference under test is the decode.
        let mut cmd2 = emit();
        let lossy = output_bounded(
            &mut cmd2,
            std::time::Duration::from_secs(20),
            std::time::Duration::from_secs(2),
        )
        .expect("bounded run");
        assert!(
            lossy.stdout.contains('\u{FFFD}'),
            "the String wrapper should have replaced the invalid byte, got {:?}",
            lossy.stdout
        );
        let _ = std::fs::remove_file(dir.join(&name));
    }

    /// The deadline must actually fire and report `timed_out`, converting a wedged child into a
    /// RETURN rather than a permanent park. This is the property the whole lock fix rests on: a
    /// caller holding `transaction.lock` can only release it by returning.
    #[test]
    fn bounded_run_returns_on_deadline_instead_of_blocking() {
        // Reuse the MEASURED long-lived fixture rather than `ping`: this module's own notes record
        // that `ping` dies with its wrapper here, and it may not even be on PATH — either way the
        // child would exit immediately and the deadline would never be exercised.
        let mut cmd = shell(&sleeper_command());
        let start = std::time::Instant::now();
        let out = output_bounded_bytes(
            &mut cmd,
            std::time::Duration::from_millis(700),
            std::time::Duration::from_secs(2),
        )
        .expect("bounded run must return, not block");
        let elapsed = start.elapsed();
        assert!(
            out.timed_out,
            "a 40s command under a 0.7s deadline must report timed_out"
        );
        assert_eq!(out.code, None, "a killed child has no exit code");
        assert!(!out.success(), "a timed-out run must not look successful");
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "returned only after {elapsed:?} — the deadline did not preempt the child"
        );
    }
}
