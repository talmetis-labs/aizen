//! Test-only serialization for the process-global native-effects gate.
//!
//! `workspace_txn::GLOBAL_HELD` is a per-PROCESS singleton: production deliberately allows one
//! native-effects holder per process, so two tests acquiring that gate concurrently conflict even
//! though each owns a private fixture directory. `with_test_lock_home` cannot help — it redirects
//! lock PATHS (thread-local) and the conflicting state is an in-process registry, not a file.
//!
//! `config::TEST_HOME_LOCK` is the repo's existing process-wide serializer, and the `workspace_txn`
//! tests that take the same gate already hold it. This wrapper adds the one property those call sites
//! need but a bare `Mutex` lacks: REENTRANCY. A plain `MutexGuard` held for a fixture's whole
//! lifetime deadlocks the moment a test creates two fixtures (shadowing does not drop the first
//! binding), and it hangs silently — the worst failure mode for a test suite.

// True while THIS thread holds the process-wide test lock, so a nested acquisition does not
// self-deadlock on a non-reentrant `Mutex`.
#[cfg(test)]
thread_local! {
    static HOLDS_GATE_LOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Reentrant, process-wide serialization for tests that acquire the global native-effects gate.
///
/// The first acquisition on a thread takes `TEST_HOME_LOCK`; nested acquisitions on that same thread
/// are no-ops, and only the outermost release gives the lock back. Cross-thread behaviour is still
/// strict mutual exclusion, which is what actually serializes the gate.
#[cfg(test)]
pub(crate) struct GateTestLock {
    /// `None` for a nested acquisition (the outermost guard owns the real lock).
    _guard: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
impl GateTestLock {
    pub(crate) fn acquire() -> Self {
        if HOLDS_GATE_LOCK.with(|holds| holds.replace(true)) {
            return Self { _guard: None };
        }
        let guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Self {
            _guard: Some(guard),
        }
    }
}

#[cfg(test)]
impl Drop for GateTestLock {
    fn drop(&mut self) {
        // Only the outermost guard releases the thread-local flag, so an inner drop cannot let a
        // sibling acquisition on this thread skip the real lock while it is still held.
        if self._guard.is_some() {
            HOLDS_GATE_LOCK.with(|holds| holds.set(false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GateTestLock;

    /// The regression this guards: a fixture held for a test's whole lifetime must not deadlock when
    /// a second fixture is created in the same test. A plain `MutexGuard` hung here silently.
    #[test]
    fn nested_acquisitions_on_one_thread_do_not_deadlock() {
        let outer = GateTestLock::acquire();
        let inner = GateTestLock::acquire();
        let deeper = GateTestLock::acquire();
        drop(deeper);
        drop(inner);
        drop(outer);
        // And the lock is genuinely released afterwards.
        let again = GateTestLock::acquire();
        drop(again);
    }

    /// Cross-thread behaviour must stay strict mutual exclusion: that is what actually serializes the
    /// process-global gate. A second thread must WAIT for the first to release.
    #[test]
    fn another_thread_waits_for_the_holder() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let acquired = Arc::new(AtomicBool::new(false));
        let holder = GateTestLock::acquire();
        let got = Arc::clone(&acquired);
        let other = std::thread::spawn(move || {
            let _lock = GateTestLock::acquire();
            got.store(true, Ordering::Release);
        });
        // The other thread cannot have taken it while this one holds it.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !acquired.load(Ordering::Acquire),
            "a second thread must not enter while the gate lock is held"
        );
        drop(holder);
        other.join().unwrap();
        assert!(acquired.load(Ordering::Acquire));
    }
}
