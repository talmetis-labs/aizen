//! Background process pool (the `process` tool). `shell_run` is FOREGROUND with a wall-clock kill
//! (120s by default) — useless for a `npm run dev`, a watcher, or a long build. `process` spawns a
//! command in the BACKGROUND, returns a `proc_<n>` handle immediately, and lets the agent
//! poll/log/wait/kill it later. Pure-Rust over `std::process` + drain threads (the same lossy-decode +
//! `chcp 65001` posture as `shell_run`); NO Python reader-threads, NO docker/ssh/modal backends —
//! local only.
//!
//! Safety: the command runs CONFINED to the cwd root (same `confine` jail as `shell_run`), and the
//! hard `cmd_guard` floor is applied to `action=start` in `agent::execute_one` BEFORE approval — so a
//! background `rm -rf /` is refused exactly like a foreground one.
//!
//! SCOPING: every handle belongs to the [`crate::core::exec_ctx::ExecutionContext::resource_scope`]
//! that created it, and the pool is only visible within that scope. The tool is granted to `coder`
//! and `tester` sub-agents (a timed-out `shell_run` tells them to come here), and those children run
//! concurrently on one shared worktree — a global view would let one child `kill` a sibling's dev
//! server, or read a build log it never started. A foreign handle reports the same "no such process"
//! as an unknown one, so a sibling's existence does not leak either. [`kill_all`] stays scope-blind:
//! process exit must reap everything.
//!
//! Lifetime: every process is spawned into a [`crate::core::proctree`] containment (Windows job
//! object / Unix process group), so `kill` reaps the whole tree and — on Windows — an Aizen crash
//! reaps it too, because the kernel closes the job handle. A dev server started here cannot outlive
//! the CLI holding its port.

use crate::agent::builtin::confine;
use crate::agent::tools::{Tool, WorkspaceEffect};
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Max concurrent/retained processes; a new `start` over the cap prunes the oldest FINISHED one.
/// Deliberately GLOBAL rather than per-scope: it bounds real OS resources, and a fan-out of children
/// each entitled to its own 16 slots is exactly the runaway this guards.
const MAX_PROCESSES: usize = 16;
/// Rolling merged stdout+stderr buffer per process (bytes); older output is dropped from the front.
const OUTPUT_CAP: usize = 200 * 1024;

/// A bounded byte buffer: appends keep only the most-recent `cap` bytes (a coarse ring).
///
/// `start_offset` is the ABSOLUTE position of `bytes[0]` in the process's whole output stream, so it
/// keeps counting up as the front is dropped. That is what makes an incremental read possible: a
/// caller holding a cursor can be handed only the bytes it has not seen, and can be told exactly how
/// many were evicted before it came back — instead of re-reading the full 200 KiB ring every poll.
struct RingBuf {
    bytes: Vec<u8>,
    cap: usize,
    start_offset: u64,
}

/// One incremental read of a [`RingBuf`], as produced by [`RingBuf::since`].
struct LogDelta {
    /// The new bytes, lossily decoded (same posture as `shell_run`: keep the ASCII structure rather
    /// than dropping a whole non-UTF-8 log).
    text: String,
    /// Cursor to pass back next time.
    next_cursor: u64,
    /// Bytes that existed after the caller's cursor but were already evicted from the ring.
    dropped_before: u64,
    /// The cursor pointed past the end of the stream (a stale handle, or a caller's arithmetic slip).
    clamped: bool,
}

impl RingBuf {
    fn new(cap: usize) -> Self {
        RingBuf {
            bytes: Vec::new(),
            cap,
            start_offset: 0,
        }
    }
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > self.cap {
            let overflow = self.bytes.len() - self.cap;
            self.bytes.drain(..overflow);
            self.start_offset += overflow as u64;
        }
    }
    /// Absolute offset one past the last retained byte — the cursor a caller resumes from.
    fn end_offset(&self) -> u64 {
        self.start_offset + self.bytes.len() as u64
    }
    /// The whole retained buffer (the no-cursor read).
    fn text(&self) -> String {
        let body = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.start_offset > 0 {
            format!("…[{} earlier bytes dropped]…\n{body}", self.start_offset)
        } else {
            body
        }
    }
    /// Only the bytes at or after `cursor`. A cursor below the retained window reports how much was
    /// lost rather than silently presenting a gap as continuous output; a cursor past the end is
    /// clamped and flagged rather than being an error (a process can be re-read after it exits).
    fn since(&self, cursor: u64) -> LogDelta {
        let end = self.end_offset();
        if cursor > end {
            return LogDelta {
                text: String::new(),
                next_cursor: end,
                dropped_before: 0,
                clamped: true,
            };
        }
        let dropped_before = self.start_offset.saturating_sub(cursor);
        let from = cursor.max(self.start_offset);
        let idx = (from - self.start_offset) as usize;
        LogDelta {
            text: String::from_utf8_lossy(&self.bytes[idx..]).into_owned(),
            next_cursor: end,
            dropped_before,
            clamped: false,
        }
    }
}

/// Set the exit flag and wake every [`Process`] `wait` blocked on this entry. Called from the monitor
/// thread when the child exits, and from [`kill_tree`] so a kill wakes its waiter at once instead of
/// letting it sit out the remaining timeout.
fn signal_done(notify: &(Mutex<bool>, Condvar)) {
    let mut flag = notify.0.lock().unwrap_or_else(|e| e.into_inner());
    *flag = true;
    notify.1.notify_all();
}

/// Record the first completion time before publishing `done`. Completion may race between the
/// monitor and a kill, but later observations must never extend the process's displayed run time.
fn mark_finished(finished_at: &Mutex<Option<Instant>>, done: &AtomicBool, now: Instant) {
    let mut finished = finished_at.lock().unwrap_or_else(|e| e.into_inner());
    finished.get_or_insert(now);
    done.store(true, Ordering::Release);
}

/// Running processes advance with `now`; completed processes retain their first completion time.
fn elapsed_duration(started: Instant, finished_at: Option<Instant>, now: Instant) -> Duration {
    finished_at
        .unwrap_or(now)
        .saturating_duration_since(started)
}

/// Actual workers, not observer/registry entries, own this lifetime. Unknown settlement is moved
/// into a process-long quarantine on final drop; this intentionally fails closed until CLI exit.
struct ProcessOwnership {
    _workspace_activities: Vec<Arc<crate::core::workspace_txn::WorkspaceActivity>>,
    _writer_leases: Vec<crate::core::workspace_txn::WorkspaceWriterLease>,
    _physical_effect: Option<crate::core::workspace_txn::PhysicalEffectLease>,
    strict_unknown: bool,
    _containment: Arc<crate::core::proctree::Containment>,
    _sandbox: Arc<Mutex<crate::sandbox::runner::SpawnGuard>>,
    _child: Arc<Mutex<Child>>,
}

/// Bounded ownership coordinator for one opaque body. `drain` tokens cover actual pipe/wait
/// workers; register all tokens before monitoring can publish tree settlement. Only a real tree
/// proof may call `prove_tree_settled`. Final unproven drop invokes the caller's quarantine handler.
/// This does not acquire admission or authorize a second body to clone an existing physical gate.
pub(crate) struct SettlementLifetime<T> {
    ownership: Mutex<Option<T>>,
    tree_settled: AtomicBool,
    drains: AtomicUsize,
    /// Set exactly once, at the moment proven settlement actually drops `ownership`. Deliberately NOT
    /// set when the ownership is quarantined instead — a retained owner must stay observable.
    released: AtomicBool,
    /// Run once, on that same proven release. This is how a startup that handed a child to a monitor
    /// learns that the child is really gone (see [`UnsettledOwnership`]).
    release_hooks: Mutex<Vec<Box<dyn FnOnce() + Send + Sync>>>,
    quarantine: fn(T),
}

impl<T> SettlementLifetime<T> {
    pub(crate) fn new(ownership: T, quarantine: fn(T)) -> Self {
        Self {
            ownership: Mutex::new(Some(ownership)),
            tree_settled: AtomicBool::new(false),
            drains: AtomicUsize::new(0),
            released: AtomicBool::new(false),
            release_hooks: Mutex::new(Vec::new()),
            quarantine,
        }
    }

    pub(crate) fn prove_tree_settled(&self) {
        self.tree_settled.store(true, Ordering::Release);
        self.release_if_settled();
    }

    pub(crate) fn drain(self: &Arc<Self>) -> DrainLifetime<T> {
        self.drains.fetch_add(1, Ordering::AcqRel);
        DrainLifetime {
            lifetime: Arc::clone(self),
        }
    }

    /// Park `hook` to run when this lifetime PROVES release. Returns `true` when it was parked for a
    /// later release, `false` when the ownership had already been released (the hook ran inline).
    ///
    /// A quarantined lifetime never releases, so its hook never runs — that is the fail-closed case,
    /// and callers must keep whatever the hook was going to clear charged until process exit.
    pub(crate) fn on_release(&self, hook: impl FnOnce() + Send + Sync + 'static) -> bool {
        if self.released.load(Ordering::Acquire) {
            hook();
            return false;
        }
        self.release_hooks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Box::new(hook));
        // A release can complete between the check and the push; run whatever is still parked. When
        // the release got there first it already drained this vector, so this is a no-op then.
        if self.released.load(Ordering::Acquire) {
            self.run_release_hooks();
        }
        true
    }

    fn release_if_settled(&self) {
        if self.tree_settled.load(Ordering::Acquire) && self.drains.load(Ordering::Acquire) == 0 {
            // Drop outside the ownership mutex. Completed registry rows keep output, not exclusion.
            let ownership = self
                .ownership
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(ownership) = ownership {
                drop(ownership);
                self.released.store(true, Ordering::Release);
                self.run_release_hooks();
            }
        }
    }

    fn run_release_hooks(&self) {
        let hooks: Vec<Box<dyn FnOnce() + Send + Sync>> = self
            .release_hooks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect();
        for hook in hooks {
            hook();
        }
    }
}

/// Count of local sidecar ownerships whose physical settlement is still unproven.
///
/// The rule this encodes: an admitted child blocks new work only once nobody is left to report its
/// outcome. Every child is charged while it starts, but a start that is still IN FLIGHT is ignored —
/// its own caller is awaiting it and will settle it, so a slow cold index must never block the
/// session. A start that FAILS is promoted to blocking, and only [`SettlementLifetime`]'s PROVEN
/// release (tree proof + every drain token returned) retires it.
///
/// A quarantined lifetime never releases, so its charge stands until process exit. That asymmetry is
/// the whole point: a transient failed start stops blocking as soon as its teardown is proven, while
/// a child nobody can account for keeps blocking.
#[derive(Default)]
pub(crate) struct UnsettledOwnership {
    /// Every outstanding charge, in flight or failed.
    outstanding: AtomicUsize,
    /// The subset still being awaited by a live caller. `outstanding - in_flight` is what blocks.
    in_flight: AtomicUsize,
    /// Charges with no proof path at all (a quarantined spawn error, a lost observer task). These
    /// block immediately and are never waited on — nothing is in flight to wait for.
    permanent: AtomicUsize,
    /// When the most recent failed charge was taken, so a caller can tell "teardown is probably still
    /// running" from "this will never settle" and wait only in the first case.
    last_failure: Mutex<Option<Instant>>,
}

/// One child's settlement charge. Held while the child starts; the holder either retires it (a live
/// owner took over) or marks it failed (nobody owns it now). Dropping it unresolved marks it failed.
///
/// Two counters move independently, and each moves exactly once:
/// * `settled` — this charge is done being outstanding. Set by a live owner's release OR by the
///   lifetime's proven release, whichever lands first.
/// * `resolved` — the holder has decided. Set by any of release / mark_failed / unresolved drop.
///
/// That split is what keeps a failed child BLOCKING (`outstanding` minus `in_flight` stays 1) until
/// its monitor actually proves the tree exited, instead of the failure quietly cancelling the count.
pub(crate) struct UnsettledCharge {
    owner: Arc<UnsettledOwnership>,
    settled: Arc<AtomicBool>,
    resolved: Arc<AtomicBool>,
}

impl UnsettledCharge {
    /// A live, verified owner now holds this child, so it no longer counts as unproven.
    pub(crate) fn release(&self) {
        if !self.resolved.swap(true, Ordering::AcqRel) {
            self.owner.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
        self.settle();
    }

    /// Nobody owns this child now (startup failed or its observer went away). It must block new work
    /// until its lifetime proves the tree exited.
    pub(crate) fn mark_failed(&self) {
        if !self.resolved.swap(true, Ordering::AcqRel) {
            self.owner.in_flight.fetch_sub(1, Ordering::AcqRel);
            *self
                .owner
                .last_failure
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(Instant::now());
        }
    }

    fn settle(&self) {
        if !self.settled.swap(true, Ordering::AcqRel) {
            self.owner.decrement();
        }
    }
}

impl Drop for UnsettledCharge {
    fn drop(&mut self) {
        // Dropping an unresolved charge means the code path that held it went away without deciding —
        // the conservative reading is "nobody owns this child", never "settled".
        self.mark_failed();
    }
}

impl UnsettledOwnership {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Does anything block new work right now? An in-flight start does not: its own caller is
    /// awaiting it and will settle it, so a slow cold index must never block the session.
    pub(crate) fn is_unsettled(&self) -> bool {
        let outstanding = self.outstanding.load(Ordering::Acquire);
        let in_flight = self.in_flight.load(Ordering::Acquire);
        outstanding.saturating_sub(in_flight) > 0 || self.permanent.load(Ordering::Acquire) > 0
    }

    /// Charge a child as it starts. The returned charge is held by the starter: retire it with
    /// [`UnsettledCharge::release`] once a live owner holds the child, or [`UnsettledCharge::mark_failed`]
    /// when startup failed. Dropping it unresolved marks it failed.
    pub(crate) fn track<T>(
        self: &Arc<Self>,
        lifetime: &Arc<SettlementLifetime<T>>,
    ) -> UnsettledCharge {
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let settled = Arc::new(AtomicBool::new(false));
        let resolved = Arc::new(AtomicBool::new(false));
        let hook_settled = Arc::clone(&settled);
        let hook_owner = Arc::clone(self);
        // A proven release settles the charge — unless a live owner already did, in which case the
        // hook must not decrement a second time and cancel another child's charge.
        //
        // Both outcomes of `on_release` are already correct, so neither needs compensating here: when
        // it parks the hook (true), the later release runs it; when it runs the hook inline (false),
        // the child had already settled and the hook has just undone the increments above.
        let _ = lifetime.on_release(move || {
            if !hook_settled.swap(true, Ordering::AcqRel) {
                hook_owner.decrement();
            }
        });
        UnsettledCharge {
            owner: Arc::clone(self),
            settled,
            resolved,
        }
    }

    /// Count ownership with no proof path at all (a quarantined spawn error, a lost observer task).
    /// Blocks immediately and is never waited on.
    pub(crate) fn charge_permanent(self: &Arc<Self>) {
        self.permanent.fetch_add(1, Ordering::AcqRel);
    }

    fn decrement(&self) {
        // Saturating: a double release must never wrap the count back to "settled".
        let _ = self
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            });
    }

    /// Wait a BOUNDED time for outstanding FAILED charges to clear, but only while they are FRESH: a
    /// start that just failed is usually mid-teardown, whereas an older charge means a quarantined
    /// owner that will never settle. Returns whether nothing is blocking.
    pub(crate) fn wait_for_recent_settlement(&self, budget: Duration) -> bool {
        if !self.is_unsettled() {
            return true;
        }
        if self.permanent.load(Ordering::Acquire) > 0 {
            return false; // no proof path at all — waiting could only waste the budget
        }
        let failed_at = *self
            .last_failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(failed_at) = failed_at else {
            return false;
        };
        let age = failed_at.elapsed();
        if age >= budget {
            return false;
        }
        let deadline = Instant::now() + (budget - age);
        while self.is_unsettled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        !self.is_unsettled()
    }
}

pub(crate) struct DrainLifetime<T> {
    lifetime: Arc<SettlementLifetime<T>>,
}

impl<T> Drop for DrainLifetime<T> {
    fn drop(&mut self) {
        self.lifetime.drains.fetch_sub(1, Ordering::AcqRel);
        self.lifetime.release_if_settled();
    }
}

impl<T> Drop for SettlementLifetime<T> {
    fn drop(&mut self) {
        if let Some(ownership) = self
            .ownership
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            if self.tree_settled.load(Ordering::Acquire) {
                drop(ownership);
            } else {
                (self.quarantine)(ownership);
            }
        }
    }
}

type ProcessLifetime = SettlementLifetime<ProcessOwnership>;
static QUARANTINE: Lazy<Mutex<Vec<ProcessOwnership>>> = Lazy::new(|| Mutex::new(Vec::new()));

fn quarantine_process(ownership: ProcessOwnership) {
    if ownership.strict_unknown {
        QUARANTINE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(ownership);
    }
    // With scoped overlap disabled and no supported tree proof, retain legacy serial lifecycle
    // instead of a permanent app-wide lock. This is NOT a claim of physical settlement.
}

struct ProcEntry {
    command: String,
    pid: u32,
    started: Instant,
    /// Execution scope that started this process; only that scope can see or touch the handle.
    scope: String,
    out: Arc<Mutex<RingBuf>>,
    exit: Arc<Mutex<Option<i32>>>,
    done: Arc<AtomicBool>,
    /// Exit notification. The `done` atomic stays for cheap non-blocking reads (`status`, `list`);
    /// this pair is what lets `wait` BLOCK until exit instead of polling on a sleep.
    notify: Arc<(Mutex<bool>, Condvar)>,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    finished_at: Arc<Mutex<Option<Instant>>>,
    /// Job object / process group holding the whole tree. Kept for the entry's lifetime: on Windows
    /// this is also crash insurance — if Aizen dies without running `kill_all`, the OS closes the job
    /// handle and the kernel reaps every descendant, so a `npm run dev` cannot outlive the CLI.
    containment: Arc<crate::core::proctree::Containment>,
    /// Sandbox bookkeeping: finishes the audit line on exit/kill and keeps the run's private temp
    /// directory alive for as long as the process may use it.
    sandbox: Arc<Mutex<crate::sandbox::runner::SpawnGuard>>,
    /// Shared with monitor and every pipe worker. Registry pruning cannot release live workers,
    /// and unknown tree settlement retains all exclusion in process-long quarantine.
    _lifetime: Arc<ProcessLifetime>,
    #[cfg(test)]
    fixture_root: PathBuf,
}

impl ProcEntry {
    fn elapsed(&self) -> Duration {
        let finished_at = *self.finished_at.lock().unwrap_or_else(|e| e.into_inner());
        elapsed_duration(self.started, finished_at, Instant::now())
    }

    fn status_label(&self) -> String {
        if self.done.load(Ordering::Acquire) {
            match *self.exit.lock().unwrap_or_else(|e| e.into_inner()) {
                Some(code) => format!("exited({code})"),
                None => "exited(?)".to_string(),
            }
        } else {
            "running".to_string()
        }
    }
}

static REGISTRY: Lazy<Mutex<HashMap<String, ProcEntry>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The execution scope of the CALLING tool body. Read per call, not per tool instance: the executor
/// seeds the context into a thread-local INSIDE the `spawn_blocking` closure, so one registered tool
/// serves the top-level turn and every delegated child, each seeing only its own handles.
fn caller_scope() -> String {
    crate::core::exec_ctx::current()
        .map(|c| c.resource_scope())
        .unwrap_or_else(|| "default".to_string())
}

/// Resolve a handle the caller OWNS. A handle from another scope is reported exactly like an unknown
/// id — a sibling dispatch must not learn that another child's process exists.
fn lookup<'a>(reg: &'a HashMap<String, ProcEntry>, id: &str, scope: &str) -> Result<&'a ProcEntry> {
    match reg.get(id) {
        Some(e) if e.scope == scope => Ok(e),
        _ => bail!("no such process '{id}'"),
    }
}

fn elapsed_label(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// Render an incremental read: a `next_cursor` the caller feeds back, then only the new bytes. Any
/// eviction or clamp is stated inline, so an incremental view is never mistaken for a complete one.
fn render_delta(id: &str, status: &str, cursor: u64, d: &LogDelta) -> String {
    let mut head = format!("{id} [{status}] next_cursor={}", d.next_cursor);
    if d.clamped {
        head.push_str(&format!(
            " (cursor {cursor} is past the end of this output — nothing new)"
        ));
    }
    if d.dropped_before > 0 {
        head.push_str(&format!(
            " ({} bytes after your cursor were dropped from the retained buffer)",
            d.dropped_before
        ));
    }
    let body = d.text.trim_end();
    if body.is_empty() {
        format!("{head}\nno new output since cursor {cursor}")
    } else {
        format!("{head}\n{body}")
    }
}

/// Spawn `command` in the background, confined to `root` (optionally a `cwd` subdir), owned by
/// `scope`. Returns the new `proc_<n>` id.
fn start(
    root: &Path,
    command: &str,
    cwd: Option<&str>,
    scope: &str,
    network: bool,
) -> Result<String> {
    let dir = match cwd {
        Some(c) => confine(root, c, true)?,
        None => root.to_path_buf(),
    };

    // Prune a finished slot if we're at the cap; refuse if everything is still running. Scope-blind
    // on purpose: the cap counts live OS processes, not per-agent entitlements. A finished entry is
    // reclaimable whoever started it — its output has already been reported to its own scope.
    {
        let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        if reg.len() >= MAX_PROCESSES {
            let victim = reg
                .iter()
                .filter(|(_, e)| e.done.load(Ordering::Acquire))
                .min_by_key(|(_, e)| e.finished_at.lock().map(|g| *g).unwrap_or(None))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    reg.remove(&k);
                }
                None => bail!(
                    "process pool full ({MAX_PROCESSES} running) — kill one first (process action=kill)"
                ),
            }
        }
    }

    // Strong lifetime admission is feature-gated until every platform can prove settlement.
    // Gate OFF keeps legacy serial background behavior, not a false tree-proof claim.
    let strict_unknown = crate::agent::workflow::SCOPED_NATIVE_PARALLEL_ENABLED;
    let (workspace_activities, writer_leases, physical_effect) = if strict_unknown {
        let context = crate::core::exec_ctx::current();
        let mut activities = context
            .as_ref()
            .map(|ctx| ctx.workspace_activities())
            .unwrap_or_default();
        activities.push(crate::core::workspace_txn::WorkspaceActivity::begin(
            root, scope,
        )?);
        let mut writers = context
            .as_ref()
            .map(|ctx| ctx.writer_leases())
            .unwrap_or_default();
        writers.push(
            crate::core::workspace_txn::WorkspaceWriterLease::acquire_scoped(
                root,
                Duration::from_secs(15),
                crate::core::cancel::current().as_ref(),
                "background process lifetime",
                scope,
            )?,
        );
        // Always fresh admission for ONE physical body, never ancestor/scope reentry.
        let physical = crate::core::workspace_txn::PhysicalEffectLease::exclusive(
            Duration::from_secs(15),
            crate::core::cancel::current().as_ref(),
            "background process body",
            scope,
        )?;
        (activities, writers, Some(physical))
    } else {
        (Vec::new(), Vec::new(), None)
    };

    // One construction path with `shell_run`: the sandbox runner wraps the shell, scrubs the
    // environment and applies policy — going background must never sidestep the sandbox.
    let mut sbx = crate::sandbox::runner::prepare_std(
        crate::sandbox::request::SandboxRequest::shell(
            crate::sandbox::CommandOrigin::ProcessStart,
            command,
            dir.clone(),
            root.to_path_buf(),
        )
        .network(network)
        .background(true),
    )?;
    sbx.command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Contain the tree at spawn. A background command is the WORST case for the orphan problem this
    // guards: `process start` is what runs dev servers and watchers, i.e. exactly the processes that
    // spawn real children behind the `cmd.exe`/`sh` wrapper and keep holding a port after a kill.
    let started = Instant::now();
    let (mut child, containment) = match sbx.spawn_contained() {
        Ok(spawned) => spawned,
        Err(error) => {
            if !error.cleanup_verified() {
                let note = error.to_string();
                // Cleanup intention is not settlement. Keep the just-created child/job and every
                // admitted workspace/physical owner until CLI exit when cleanup is unproven.
                std::mem::forget((
                    error,
                    workspace_activities,
                    writer_leases,
                    physical_effect,
                    sbx,
                ));
                return Err(
                    anyhow::Error::new(crate::agent::tools::ToolEffectsUnsettled(
                        crate::agent::tools::ToolEffectState::UnknownWorker,
                    ))
                    .context(note),
                );
            }
            return Err(error).with_context(|| format!("spawning background `{command}`"));
        }
    };
    let containment = Arc::new(containment);
    let sandbox = Arc::new(Mutex::new(sbx.into_guard()));
    let pid = child.id();

    let out = Arc::new(Mutex::new(RingBuf::new(OUTPUT_CAP)));
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let stdin = child.stdin.take();
    let child = Arc::new(Mutex::new(child));
    let lifetime = Arc::new(ProcessLifetime::new(
        ProcessOwnership {
            _workspace_activities: workspace_activities,
            _writer_leases: writer_leases,
            _physical_effect: physical_effect,
            strict_unknown,
            _containment: Arc::clone(&containment),
            _sandbox: Arc::clone(&sandbox),
            _child: Arc::clone(&child),
        },
        quarantine_process,
    ));
    // Seat both drain tokens before starting either worker: one fast EOF must not cause early
    // physical settlement before the other pipe has even been registered.
    let stdout_lifetime = lifetime.drain();
    let stderr_lifetime = lifetime.drain();
    spawn_drain(out_pipe, Arc::clone(&out), stdout_lifetime);
    spawn_drain(err_pipe, Arc::clone(&out), stderr_lifetime);

    let exit = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let notify = Arc::new((Mutex::new(false), Condvar::new()));
    let finished_at = Arc::new(Mutex::new(None));

    // Monitor: reap the child when it exits, recording the code + done flag (no busy CPU), then wake
    // any `wait`. `done` is stored BEFORE the notify so a woken waiter always observes the exit.
    {
        let child = Arc::clone(&child);
        let exit = Arc::clone(&exit);
        let done = Arc::clone(&done);
        let notify = Arc::clone(&notify);
        let finished_at = Arc::clone(&finished_at);
        let sandbox = Arc::clone(&sandbox);
        let containment = Arc::clone(&containment);
        let lifetime = Arc::clone(&lifetime);
        std::thread::spawn(move || {
            loop {
                if done.load(Ordering::Acquire) {
                    signal_done(&notify);
                    break;
                }
                let st = { child.lock().unwrap_or_else(|e| e.into_inner()).try_wait() };
                match st {
                    Ok(Some(status)) => {
                        *exit.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(status.code().unwrap_or(-1));
                        mark_finished(&finished_at, &done, Instant::now());
                        sandbox
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .finish(crate::sandbox::runner::Outcome::Exit(status.code()));
                        signal_done(&notify);
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(60)),
                    Err(_) => {
                        // UI observer completion is not proof of physical completion.
                        mark_finished(&finished_at, &done, Instant::now());
                        signal_done(&notify);
                        return;
                    }
                }
            }
            loop {
                match containment.known_quiescent() {
                    Ok(Some(true)) => {
                        lifetime.prove_tree_settled();
                        break;
                    }
                    Ok(Some(false)) if strict_unknown => {
                        std::thread::sleep(Duration::from_millis(60))
                    }
                    // Gate-OFF observers retain legacy lifecycle without a permanent monitor.
                    Ok(Some(false)) => break,
                    // None/Unix detached descendants/errors cannot prove settlement. The final
                    // lifetime drop quarantines exclusion instead of silently admitting natives.
                    Ok(None) | Err(_) => break,
                }
            }
        });
    }

    let id = format!("proc_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let entry = ProcEntry {
        command: command.to_string(),
        pid,
        started,
        scope: scope.to_string(),
        out,
        exit,
        done,
        notify,
        child,
        stdin: Arc::new(Mutex::new(stdin)),
        finished_at,
        containment,
        sandbox,
        _lifetime: lifetime,
        #[cfg(test)]
        fixture_root: root.to_path_buf(),
    };
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), entry);
    Ok(id)
}

/// Read a pipe in chunks on a background thread, appending raw bytes to the shared ring buffer.
fn spawn_drain<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
    out: Arc<Mutex<RingBuf>>,
    lifetime: DrainLifetime<ProcessOwnership>,
) {
    if let Some(mut p) = pipe {
        std::thread::spawn(move || {
            let _lifetime = lifetime;
            let mut buf = [0u8; 8192];
            loop {
                match p.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(&buf[..n]),
                }
            }
        });
    }
}

/// Best-effort kill of a process AND its children (a bg shell usually has a real child).
///
/// Uses the job object / process group captured at spawn rather than the old `taskkill /F /T /PID`.
/// `taskkill /T` walks the live parent→child chain, so it only reaches descendants whose ancestors are
/// *still running*: once the `cmd.exe` wrapper exits (or a launcher double-forks, as npm/python
/// wrappers do), the chain is broken and the real server survives — holding its port and the pipe. The
/// kernel's job membership has no such gap, and needs no extra process spawn to do the killing.
fn kill_tree(entry: &ProcEntry) {
    let mut child = entry.child.lock().unwrap_or_else(|e| e.into_inner());
    crate::core::proctree::kill_tree(&mut child, &entry.containment);
    mark_finished(&entry.finished_at, &entry.done, Instant::now());
    entry
        .sandbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .finish(crate::sandbox::runner::Outcome::Exit(None));
    // Wake a `wait` immediately rather than leaving it to sit out its remaining timeout.
    signal_done(&entry.notify);
}

/// Kill every still-running background process and clear the pool. Called on REPL + daemon exit so
/// dev servers / watchers the agent started via `process start` don't outlive the CLI (orphaned
/// ports + CPU). Deliberately SCOPE-BLIND — process exit must reap every scope, not just whichever
/// one happens to be pinned on the calling thread. Best-effort and idempotent.
pub fn kill_all() {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    for entry in reg.values() {
        if !entry.done.load(Ordering::Acquire) {
            kill_tree(entry);
        }
    }
    reg.clear();
}

/// One background process, as the jobs panel shows it: the handle, whether it is still running, how
/// long it has run, and the command. A pure snapshot — copied out under the registry lock so the UI
/// never holds it.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: String,
    pub running: bool,
    /// Human elapsed, e.g. `7m03s` (or `3s`). For a finished process this is its total run time.
    pub elapsed: String,
    pub command: String,
}

/// A snapshot of every background process this process has started, newest handle last. Scope-blind
/// on purpose: the jobs panel is a whole-app view, so a sub-agent's dev server is visible from the top
/// level (its output stays scope-gated — the panel only names it). Running processes sort first so the
/// interesting rows are on top; the caller decides how many rows to show.
pub fn snapshot() -> Vec<JobInfo> {
    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let mut rows: Vec<JobInfo> = reg
        .iter()
        .map(|(id, e)| JobInfo {
            id: id.clone(),
            running: !e.done.load(Ordering::Acquire),
            elapsed: elapsed_label(e.elapsed()),
            command: e.command.clone(),
        })
        .collect();
    // Running first, then by handle number (proc_2 before proc_10) so the order is stable.
    rows.sort_by(|a, b| {
        b.running
            .cmp(&a.running)
            .then_with(|| numeric_handle(&a.id).cmp(&numeric_handle(&b.id)))
    });
    rows
}

/// The numeric tail of a `proc_<n>` handle, for a stable sort. Unparseable → 0 (sorts first).
fn numeric_handle(id: &str) -> u64 {
    id.rsplit('_')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Kill one background process by handle from OUTSIDE the tool path — the jobs panel's stop key.
/// SCOPE-BLIND on purpose (unlike the `process` tool's `kill`, which is scoped): the panel is a
/// whole-app view, so it must be able to stop a dev server a sub-agent started, exactly as the panel
/// shows it. Returns a short human note for the caller to display.
pub fn kill_by_id(id: &str) -> String {
    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    match reg.get(id) {
        Some(e) if !e.done.load(Ordering::Acquire) => {
            kill_tree(e);
            format!("✓ stopped {id}")
        }
        Some(_) => format!("{id} already exited"),
        None => format!("no such process '{id}'"),
    }
}

/// The retained output of one background process by handle, from OUTSIDE the tool path — the jobs
/// panel's log key. SCOPE-BLIND for the same reason as [`kill_by_id`] (the panel is a whole-app view).
/// Returns a `(status, body)` pair the panel renders; an unknown handle yields a short note as the
/// body. The body is the reverse-chronological tail capped at `limit` lines, so a chatty dev server's
/// log stays readable rather than dumping the whole 200 KiB ring.
pub fn log_by_id(id: &str, limit: usize) -> Option<(String, String)> {
    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let e = reg.get(id)?;
    let status = e.status_label();
    let body = {
        let buf = e.out.lock().unwrap_or_else(|e| e.into_inner());
        buf.text()
    };
    Some((status, tail_lines(&body, limit)))
}

/// The last `limit` non-empty lines of `body`, with a leading note when earlier lines were dropped.
/// Kept here (not in the UI) so the cap and the marker are one fact the panel cannot drift from.
fn tail_lines(body: &str, limit: usize) -> String {
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= limit {
        return body.to_string();
    }
    let skip = lines.len() - limit;
    let mut out = format!("… {skip} earlier line(s) hidden (full log via process action=log)\n");
    out.push_str(&lines[skip..].join("\n"));
    out
}

/// `process` — manage long-running background commands (start/list/log/status/wait/kill/write).
pub struct Process {
    root: PathBuf,
}
impl Process {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for Process {
    fn name(&self) -> &str {
        "process"
    }
    fn description(&self) -> &str {
        "Background builds/servers beyond shell_run timeout. start returns proc_<n>; wait blocks \
         to exit, log cursor reads new output; status/kill/write manage it. Quick commands: shell_run."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["start", "list", "log", "status", "wait", "kill", "write"]},
                "command": {"type": "string", "description": "command (start)"},
                "cwd": {"type": "string", "description": "working dir (start)"},
                "network": {"type": "boolean", "description": "network, including port binding (start; approval-gated)"},
                "id": {"type": "string", "description": "proc_<n> (except start/list)"},
                "cursor": {"type": "integer", "description": "prior next_cursor: new output only (log/wait)"},
                "format": crate::agent::result_format::schema_property(),
                "timeout_secs": {"type": "integer", "description": "wait seconds (default 30)"},
                "input": {"type": "string", "description": "stdin (write)"},
                "enter": {"type": "boolean", "description": "append newline (write; default true)"}
            },
            "required": ["action"]
        })
    }
    fn is_destructive(&self) -> bool {
        true // action=start runs arbitrary commands; kill/write affect a live process
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        match args.get("action").and_then(|v| v.as_str()) {
            Some("start") | Some("write") => WorkspaceEffect::OpaqueWorkspace,
            _ => WorkspaceEffect::None,
        }
    }
    fn command_to_guard(&self, args: &Value) -> Option<String> {
        // Only `start` executes a command; kill/logs/wait/write drive an existing handle. Guarded
        // identically to `shell_run` so going background can't sidestep the hard floor.
        if args.get("action").and_then(|v| v.as_str()) != Some("start") {
            return None;
        }
        args.get("command")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .context("missing `action`")?;
        let scope = caller_scope();
        let cursor = args.get("cursor").and_then(|v| v.as_u64());
        match action {
            "start" => {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("start needs `command`")?;
                let cwd = args.get("cwd").and_then(|v| v.as_str());
                let network = args
                    .get("network")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let id = start(&self.root, command, cwd, &scope, network)?;
                let pid = REGISTRY
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id)
                    .map(|e| e.pid)
                    .unwrap_or(0);
                Ok(format!(
                    "{id} started (pid {pid}): {command}\nIt runs in the background. For a build or \
                     test, block on it with process(action=wait, id={id}, timeout_secs=…). For \
                     progress on something long-lived, poll process(action=log, id={id}, \
                     cursor=<next_cursor>) — with a cursor you get only the new output instead of \
                     the whole log again. Stop it with process(action=kill, id={id})."
                ))
            }
            "list" => {
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let mut ids: Vec<&String> = reg
                    .iter()
                    .filter(|(_, e)| e.scope == scope)
                    .map(|(k, _)| k)
                    .collect();
                if ids.is_empty() {
                    return Ok("no background processes".to_string());
                }
                ids.sort();
                let mut s = String::from("background processes:\n");
                for id in ids {
                    let e = &reg[id];
                    s.push_str(&format!(
                        "  {id}  {:<11}  {:>6}  {}\n",
                        e.status_label(),
                        elapsed_label(e.elapsed()),
                        e.command
                    ));
                }
                Ok(s.trim_end().to_string())
            }
            "log" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("log needs `id`")?;
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                let buf = e.out.lock().unwrap_or_else(|e| e.into_inner());
                let out = match cursor {
                    Some(c) => render_delta(id, &e.status_label(), c, &buf.since(c)),
                    None => format!(
                        "{id} [{}] next_cursor={}\n{}",
                        e.status_label(),
                        buf.end_offset(),
                        buf.text().trim_end()
                    ),
                };
                Ok(crate::agent::result_format::finish_log(
                    "process", args, out,
                ))
            }
            "status" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("status needs `id`")?;
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                Ok(format!(
                    "{id}: {} (elapsed {})",
                    e.status_label(),
                    elapsed_label(e.elapsed())
                ))
            }
            "wait" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("wait needs `id`")?
                    .to_string();
                let timeout = args
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30);
                // Snapshot the notification handle, then BLOCK on it without holding the registry
                // lock. A condvar rather than a sleep-poll: a `cargo check` that finishes in 800ms
                // returns in 800ms, and a kill wakes the waiter at once (see `kill_tree`).
                let (done, notify) = {
                    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                    let e = lookup(&reg, &id, &scope)?;
                    (Arc::clone(&e.done), Arc::clone(&e.notify))
                };
                let deadline = Instant::now() + Duration::from_secs(timeout);
                {
                    let mut flag = notify.0.lock().unwrap_or_else(|e| e.into_inner());
                    while !*flag {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let (guard, _) = notify
                            .1
                            .wait_timeout(flag, deadline - now)
                            .unwrap_or_else(|e| e.into_inner());
                        flag = guard;
                    }
                }
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, &id, &scope)?;
                let buf = e.out.lock().unwrap_or_else(|e| e.into_inner());
                let exited = done.load(Ordering::Acquire);
                // Cursor support on BOTH branches: a wait that times out is the natural point to
                // resume from, and re-sending the whole buffer each round is the cost this fixes.
                if let Some(c) = cursor {
                    let delta = buf.since(c);
                    let status = if exited {
                        e.status_label()
                    } else {
                        format!("still running after {timeout}s, not killed")
                    };
                    return Ok(crate::agent::result_format::finish_log(
                        "process",
                        args,
                        render_delta(&id, &status, c, &delta),
                    ));
                }
                let text = buf.text();
                let out = if exited {
                    format!(
                        "{id} {} next_cursor={}\n{}",
                        e.status_label(),
                        buf.end_offset(),
                        text.trim_end()
                    )
                } else {
                    format!(
                        "{id} still running after {timeout}s (not killed). next_cursor={} — pass it \
                         as `cursor` next time to get only new output. Latest output:\n{}",
                        buf.end_offset(),
                        text.trim_end()
                    )
                };
                Ok(crate::agent::result_format::finish_log(
                    "process", args, out,
                ))
            }
            "kill" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("kill needs `id`")?;
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                kill_tree(e);
                Ok(format!("{id} killed"))
            }
            "write" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("write needs `id`")?;
                let mut input = args
                    .get("input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if args.get("enter").and_then(|v| v.as_bool()).unwrap_or(true) {
                    input.push('\n');
                }
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                let mut guard = e.stdin.lock().unwrap_or_else(|e| e.into_inner());
                let stdin = guard.as_mut().context("process stdin is closed")?;
                stdin
                    .write_all(input.as_bytes())
                    .context("writing to process stdin")?;
                stdin.flush().ok();
                Ok(format!("wrote {} bytes to {id} stdin", input.len()))
            }
            other => bail!("unknown action '{other}' (use start/list/log/status/wait/kill/write)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock order is HOME then pool. Keep the owned home/root alive until actual ownership drops,
    /// not merely until the wrapper's `done` flag or a log observation is published.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    struct Fixture {
        _home_lock: std::sync::MutexGuard<'static, ()>,
        _serial: std::sync::MutexGuard<'static, ()>,
        env: Option<crate::core::config::EnvGuard>,
        directory: crate::core::file_placement::tests::TestDir,
        root: PathBuf,
    }

    fn serial() -> Fixture {
        let home_lock = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let directory = crate::core::file_placement::tests::TestDir::new();
        let root = directory.path().join("workspace");
        let home = directory.path().join("home");
        std::fs::create_dir(&root).unwrap();
        let env = crate::core::config::EnvGuard::set([("AIZEN_HOME", &home)]);
        Fixture {
            _home_lock: home_lock,
            _serial: serial,
            env: Some(env),
            directory,
            root,
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let owned: Vec<_> = REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(_, entry)| entry.fixture_root == self.root)
                .map(|(id, _)| id.clone())
                .collect();
            for id in owned {
                forget(&id);
            }
            drop(self.env.take());
            // The directory field is dropped after the environment is restored.
            let _ = self.directory.path();
        }
    }

    /// Run `f` with `scope` pinned as the execution scope, exactly as the executor does for a tool
    /// body inside its `spawn_blocking` closure.
    fn scoped<T>(scope: &str, f: impl FnOnce() -> T) -> T {
        let ctx = crate::core::exec_ctx::ExecutionContext::default().with_resource_scope(scope);
        crate::core::exec_ctx::with_current(ctx, f)
    }

    fn forget(id: &str) {
        let lifetime = REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|entry| Arc::clone(&entry._lifetime));
        if let Some(lifetime) = lifetime {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !lifetime.released.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(lifetime.released.load(Ordering::Acquire), "owned process {id} must prove tree/pipe settlement and release ownership before teardown");
        }
        REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    #[test]
    fn physical_settlement_waits_for_both_pipe_workers_after_registry_drop() {
        struct Owner(std::sync::mpsc::Sender<()>);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        fn quarantine(_: Owner) {
            panic!("proven fixture must not quarantine");
        }
        let (released, release_seen) = std::sync::mpsc::channel();
        let lifetime = Arc::new(SettlementLifetime::new(Owner(released), quarantine));
        let stdout = lifetime.drain();
        let stderr = lifetime.drain();
        let monitor = Arc::clone(&lifetime);
        let (started, ready) = std::sync::mpsc::channel();
        let (allow_exit, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let stdout = stdout;
            started.send(()).unwrap();
            let allowed = wait.recv_timeout(Duration::from_secs(5));
            drop(stdout);
            allowed.unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        monitor.prove_tree_settled();
        drop(lifetime); // Observer/registry detached; actual drain tokens retain exclusion.
        drop(stderr);
        assert!(matches!(
            release_seen.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        allow_exit.send(()).unwrap();
        worker.join().unwrap();
        release_seen.recv_timeout(Duration::from_secs(5)).unwrap();
        // Settled output history may retain the monitor Arc without retaining exclusion.
        drop(monitor);
    }

    #[test]
    fn unknown_tree_settlement_quarantines_ownership_on_final_worker_drop() {
        struct Owner {
            dropped: std::sync::mpsc::Sender<()>,
            quarantined: std::sync::mpsc::Sender<()>,
        }
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.dropped.send(());
            }
        }
        fn quarantine(owner: Owner) {
            owner.quarantined.send(()).unwrap();
            // This fixture carries only channel senders, not real process/lease ownership.
            std::mem::forget(owner);
        }
        let (dropped, seen_drop) = std::sync::mpsc::channel();
        let (quarantined, seen_quarantine) = std::sync::mpsc::channel();
        let lifetime = Arc::new(SettlementLifetime::new(
            Owner {
                dropped,
                quarantined,
            },
            quarantine,
        ));
        let drain = lifetime.drain();
        drop(lifetime);
        assert!(matches!(
            seen_quarantine.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(drain);
        seen_quarantine
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            seen_drop.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn elapsed_duration_running_advances_with_now() {
        let started = Instant::now();
        assert_eq!(elapsed_duration(started, None, started), Duration::ZERO);
        assert_eq!(
            elapsed_duration(started, None, started + Duration::from_secs(3)),
            Duration::from_secs(3)
        );
        assert_eq!(
            elapsed_duration(started, None, started + Duration::from_secs(63)),
            Duration::from_secs(63)
        );
    }

    #[test]
    fn elapsed_duration_finished_freezes_at_first_completion() {
        let started = Instant::now();
        let finished = started + Duration::from_secs(63);
        for now in [finished, finished + Duration::from_secs(600)] {
            assert_eq!(
                elapsed_duration(started, Some(finished), now),
                Duration::from_secs(63)
            );
        }
    }

    #[test]
    fn elapsed_duration_saturates_when_end_precedes_start() {
        let earlier = Instant::now();
        let started = earlier + Duration::from_secs(1);
        assert_eq!(elapsed_duration(started, None, earlier), Duration::ZERO);
        assert_eq!(
            elapsed_duration(started, Some(earlier), started + Duration::from_secs(60)),
            Duration::ZERO
        );
    }

    #[test]
    fn mark_finished_records_first_timestamp_before_done() {
        let finished_at = Mutex::new(None);
        let done = AtomicBool::new(false);
        let first = Instant::now();
        mark_finished(&finished_at, &done, first);
        assert!(done.load(Ordering::Acquire));
        assert_eq!(*finished_at.lock().unwrap(), Some(first));

        // A later exit/kill observation must not replace the first completion timestamp.
        mark_finished(&finished_at, &done, first + Duration::from_secs(60));
        assert!(done.load(Ordering::Acquire));
        assert_eq!(*finished_at.lock().unwrap(), Some(first));
    }

    #[test]
    fn start_log_wait_lifecycle() {
        let _s = serial();
        scoped("duration-lifecycle", || {
            let t = Process::new(_s.root.clone());
            let started = t
                .execute(&serde_json::json!({"action":"start","command":"echo hello-bg"}))
                .unwrap();
            let id = started.split_whitespace().next().unwrap().to_string();
            assert!(id.starts_with("proc_"));

            // Wait for it to finish, then the log must contain the output.
            let waited = t
                .execute(&serde_json::json!({"action":"wait","id":id,"timeout_secs":10}))
                .unwrap();
            assert!(waited.contains("hello-bg"), "got: {waited}");
            assert!(
                waited.contains("exited("),
                "should be exited; got: {waited}"
            );

            let duration = {
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, &id, "duration-lifecycle").unwrap();
                assert!(e.done.load(Ordering::Acquire));
                let finished = e.finished_at.lock().unwrap().expect("completion timestamp");
                assert!(
                    finished >= e.started,
                    "start must precede monitor completion"
                );
                let duration = elapsed_duration(e.started, Some(finished), finished);
                assert_eq!(e.elapsed(), duration);
                assert_eq!(
                    elapsed_duration(
                        e.started,
                        Some(finished),
                        finished + Duration::from_secs(600)
                    ),
                    duration
                );
                duration
            };
            let expected_elapsed = elapsed_label(duration);

            // Completion freezes duration, but retains this owned handle in history and its logs.
            let row = snapshot().into_iter().find(|row| row.id == id).unwrap();
            assert!(!row.running);
            assert_eq!(row.elapsed, expected_elapsed);
            let listed = t.execute(&serde_json::json!({"action":"list"})).unwrap();
            assert!(
                listed.contains(&id),
                "completed handle lost from list: {listed}"
            );
            assert!(listed.contains(&expected_elapsed), "got: {listed}");
            let status = t
                .execute(&serde_json::json!({"action":"status","id":id}))
                .unwrap();
            assert!(
                status.contains(&format!("elapsed {expected_elapsed}")),
                "got: {status}"
            );
            let log = t
                .execute(&serde_json::json!({"action":"log","id":id}))
                .unwrap();
            assert!(log.contains("hello-bg"), "completed log lost: {log}");
            let (panel_status, panel_log) = log_by_id(&id, 20).unwrap();
            assert!(panel_status.contains("exited("), "got: {panel_status}");
            assert!(
                panel_log.contains("hello-bg"),
                "completed panel log lost: {panel_log}"
            );

            forget(&id);
        });
    }

    #[test]
    fn unknown_id_errors() {
        let _s = serial();
        let t = Process::new(_s.root.clone());
        assert!(t
            .execute(&serde_json::json!({"action":"log","id":"proc_999999"}))
            .is_err());
        assert!(t.execute(&serde_json::json!({"action":"status"})).is_err()); // missing id
        assert!(t.execute(&serde_json::json!({"action":"bogus"})).is_err());
    }

    #[test]
    fn out_of_band_log_and_kill_by_handle() {
        let _s = serial();
        // `log_by_id` is the jobs-panel reader: it must see a scope's process without the caller
        // pinning that scope (the panel is a whole-app view).
        let id = scoped("panel-scope", || {
            Process::new(_s.root.clone())
                .execute(&serde_json::json!({"action":"start","command":"echo panel-log"}))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        });
        // Poll for the output to land (the drain thread is async).
        let mut got = None;
        for _ in 0..50 {
            if let Some((status, body)) = log_by_id(&id, 200) {
                if body.contains("panel-log") {
                    got = Some(status);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(got.is_some(), "log_by_id must read another scope's output");
        // Unknown handle → None (the panel says "already gone" rather than panicking).
        assert!(log_by_id("proc_does_not_exist", 20).is_none());
        // kill_by_id on an unknown handle is a note, not a panic.
        assert!(kill_by_id("proc_does_not_exist").contains("no such process"));
        forget(&id);
    }

    #[test]
    fn tail_lines_keeps_the_end_and_marks_what_was_hidden() {
        assert_eq!(
            tail_lines("a\nb\nc", 5),
            "a\nb\nc",
            "short bodies pass through"
        );
        let t = tail_lines("l1\nl2\nl3\nl4\nl5", 2);
        assert!(t.contains("3 earlier line(s) hidden"), "{t}");
        assert!(t.ends_with("l4\nl5"), "{t}");
        assert!(!t.contains("l1"), "the tail must drop the oldest lines");
    }

    #[test]
    fn wait_returns_as_soon_as_the_process_exits() {
        let _s = serial();
        let t = Process::new(_s.root.clone());
        let started = t
            .execute(&serde_json::json!({"action":"start","command":"echo quick"}))
            .unwrap();
        let id = started.split_whitespace().next().unwrap().to_string();
        // A generous timeout the call must NOT sit out: the condvar wakes on exit, so this returns
        // in well under a second. A regression to a long sleep-poll would blow the bound.
        let began = Instant::now();
        let waited = t
            .execute(&serde_json::json!({"action":"wait","id":id,"timeout_secs":30}))
            .unwrap();
        assert!(waited.contains("exited("), "got: {waited}");
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "wait blocked far longer than the process ran: {:?}",
            began.elapsed()
        );
        forget(&id);
    }

    #[test]
    fn log_with_a_cursor_returns_only_new_output() {
        let _s = serial();
        let t = Process::new(_s.root.clone());
        let started = t
            .execute(&serde_json::json!({"action":"start","command":"echo first-line"}))
            .unwrap();
        let id = started.split_whitespace().next().unwrap().to_string();
        let first = t
            .execute(&serde_json::json!({"action":"wait","id":id,"timeout_secs":10}))
            .unwrap();
        assert!(first.contains("first-line"), "got: {first}");

        // Feed the advertised cursor back: the process is done, so there is nothing new and the
        // earlier output must NOT be repeated.
        let cursor: u64 = first
            .split("next_cursor=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no next_cursor in: {first}"));
        let second = t
            .execute(&serde_json::json!({"action":"log","id":id,"cursor":cursor}))
            .unwrap();
        assert!(
            !second.contains("first-line"),
            "cursor read repeated old output: {second}"
        );
        assert!(second.contains("no new output"), "got: {second}");
        forget(&id);
    }

    #[test]
    fn handles_are_private_to_their_execution_scope() {
        let _s = serial();
        let t = Process::new(_s.root.clone());
        let started = scoped("iso-owner", || {
            t.execute(&serde_json::json!({"action":"start","command":"echo owned"}))
                .unwrap()
        });
        let id = started.split_whitespace().next().unwrap().to_string();

        // A sibling scope can neither see nor touch it — and gets the same message as for an id
        // that never existed, so the handle's existence does not leak.
        scoped("iso-sibling", || {
            let listed = t.execute(&serde_json::json!({"action":"list"})).unwrap();
            assert_eq!(listed, "no background processes", "sibling saw: {listed}");
            for action in ["log", "status", "wait", "kill"] {
                let err = t
                    .execute(&serde_json::json!({"action":action,"id":id,"timeout_secs":1}))
                    .expect_err("sibling scope must not resolve a foreign handle");
                assert!(
                    err.to_string().contains("no such process"),
                    "{action}: {err}"
                );
            }
        });

        // The owning scope still has it.
        scoped("iso-owner", || {
            let listed = t.execute(&serde_json::json!({"action":"list"})).unwrap();
            assert!(listed.contains(&id), "owner lost its handle: {listed}");
            assert!(t
                .execute(&serde_json::json!({"action":"log","id":id}))
                .is_ok());
        });
        forget(&id);
    }

    #[test]
    fn kill_all_reaps_every_scope() {
        let _s = serial();
        let t = Process::new(_s.root.clone());
        let a = scoped("reap-a", || {
            t.execute(&serde_json::json!({"action":"start","command":"echo a"}))
                .unwrap()
        });
        let b = scoped("reap-b", || {
            t.execute(&serde_json::json!({"action":"start","command":"echo b"}))
                .unwrap()
        });
        let (ia, ib) = (
            a.split_whitespace().next().unwrap().to_string(),
            b.split_whitespace().next().unwrap().to_string(),
        );
        let lifetimes: Vec<_> = {
            let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            [&ia, &ib]
                .iter()
                .map(|id| Arc::clone(&reg[*id]._lifetime))
                .collect()
        };
        kill_all();
        for lifetime in lifetimes {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !lifetime.released.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                lifetime.released.load(Ordering::Acquire),
                "fixture-owned killed body must settle before teardown"
            );
        }
        let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !reg.contains_key(&ia) && !reg.contains_key(&ib),
            "kill_all must clear handles from every scope"
        );
    }

    // ── RingBuf offsets ──────────────────────────────────────────────────────

    #[test]
    fn ring_offsets_track_absolute_positions() {
        let mut r = RingBuf::new(16);
        assert_eq!(r.end_offset(), 0);
        r.push(b"abcd");
        assert_eq!((r.start_offset, r.end_offset()), (0, 4));

        // A fresh cursor gets everything and lands at the end.
        let d = r.since(0);
        assert_eq!(d.text, "abcd");
        assert_eq!(d.next_cursor, 4);
        assert_eq!(d.dropped_before, 0);
        assert!(!d.clamped);

        // Only the appended bytes come back on the next read.
        r.push(b"efgh");
        let d = r.since(4);
        assert_eq!(d.text, "efgh");
        assert_eq!(d.next_cursor, 8);
    }

    #[test]
    fn ring_reports_bytes_dropped_before_a_stale_cursor() {
        let mut r = RingBuf::new(8);
        r.push(b"0123456789ABCD"); // 14 bytes into an 8-byte ring → first 6 evicted
        assert_eq!(r.start_offset, 6);
        assert_eq!(r.end_offset(), 14);
        let d = r.since(2);
        assert_eq!(d.dropped_before, 4, "bytes 2..6 are gone");
        assert_eq!(d.text, "6789ABCD", "only what is still retained");
        assert_eq!(d.next_cursor, 14);
        // The whole-buffer read says so too, so a no-cursor caller isn't misled either.
        assert!(r.text().contains("6 earlier bytes dropped"), "{}", r.text());
    }

    #[test]
    fn ring_handles_no_new_output_and_a_future_cursor() {
        let mut r = RingBuf::new(16);
        r.push(b"abc");
        // Cursor exactly at the end: nothing new, cursor unchanged.
        let d = r.since(3);
        assert!(d.text.is_empty());
        assert_eq!(d.next_cursor, 3);
        assert!(!d.clamped);
        // Beyond the end: clamped and flagged rather than an error or a panic.
        let d = r.since(999);
        assert!(d.text.is_empty());
        assert_eq!(d.next_cursor, 3);
        assert!(d.clamped);
    }
}
