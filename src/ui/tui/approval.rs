//! FIFO ownership of approval presentation, not of the tool or child execution that follows it.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct State {
    next: u64,
    queued: VecDeque<u64>,
    owner: Option<u64>,
    owner_live: bool,
    reply: Option<Sender<char>>,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct Coordinator {
    state: Mutex<State>,
    changed: Condvar,
}

pub(crate) struct Presentation<'a> {
    coordinator: &'a Coordinator,
    id: u64,
}

impl Coordinator {
    pub(crate) fn acquire(&self, interrupted: impl Fn() -> bool) -> Option<Presentation<'_>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || interrupted() {
            return None;
        }
        state.next = state.next.checked_add(1)?;
        let id = state.next;
        state.queued.push_back(id);
        self.changed.notify_all();
        loop {
            if state.closed || interrupted() || !state.queued.contains(&id) {
                state.queued.retain(|queued| *queued != id);
                self.changed.notify_all();
                return None;
            }
            if state.owner.is_none() && state.queued.front() == Some(&id) {
                state.queued.pop_front();
                state.owner = Some(id);
                state.owner_live = true;
                return Some(Presentation {
                    coordinator: self,
                    id,
                });
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// A key captured for an old menu must never approve a newer request.
    pub(crate) fn answer(&self, id: u64, answer: char) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.closed && state.owner_live && state.owner == Some(id) {
            if let Some(reply) = state.reply.take() {
                let _ = reply.send(answer);
            }
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.queued.clear();
        // Retain ownership until its UI cleanup has run, even across close/reopen.
        state.owner_live = false;
        state.reply = None;
        self.changed.notify_all();
    }

    pub(crate) fn reopen(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = false;
    }
}

impl Presentation<'_> {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn is_live(&self) -> bool {
        let state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        !state.closed && state.owner_live && state.owner == Some(self.id)
    }

    pub(crate) fn publish(&self) -> Option<Receiver<char>> {
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.closed
            || !state.owner_live
            || state.owner != Some(self.id)
            || state.reply.is_some()
        {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        state.reply = Some(tx);
        Some(rx)
    }
}

impl Drop for Presentation<'_> {
    fn drop(&mut self) {
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.owner == Some(self.id) {
            state.reply = None;
            state.owner_live = false;
            state.owner = None;
            self.coordinator.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn wait_for_queued(coordinator: &Coordinator, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut state = coordinator.state.lock().unwrap();
        while state.queued.len() < count {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "request did not enter the approval queue"
            );
            state = coordinator
                .changed
                .wait_timeout(state, remaining)
                .unwrap()
                .0;
        }
    }

    #[test]
    fn two_requests_keep_the_first_sender_and_reject_stale_answers() {
        let coordinator = Arc::new(Coordinator::default());
        let first = coordinator.acquire(|| false).unwrap();
        let first_id = first.id();
        let first_reply = first.publish().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let other = coordinator.clone();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let second = other
                .acquire(|| std::time::Instant::now() >= deadline)
                .unwrap();
            let reply = second.publish().unwrap();
            other.answer(first_id, 'y');
            assert!(matches!(
                reply.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            other.answer(second.id(), 'n');
            assert_eq!(reply.recv_timeout(Duration::from_secs(2)).unwrap(), 'n');
            drop(second);
            done_tx.send(()).unwrap();
        });
        wait_for_queued(&coordinator, 1);
        coordinator.answer(first_id, 'y');
        coordinator.answer(first_id, 'n'); // duplicate cannot replace the first decision
        assert_eq!(
            first_reply.recv_timeout(Duration::from_secs(2)).unwrap(),
            'y'
        );
        // Ownership lasts until the caller has recorded its grant and cleaned its UI.
        assert_eq!(coordinator.state.lock().unwrap().queued.len(), 1);
        drop(first);
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn cancellation_removes_a_queued_request_without_releasing_the_owner() {
        let coordinator = Coordinator::default();
        let first = coordinator.acquire(|| false).unwrap();
        let reply = first.publish().unwrap();
        // First check admits the ticket to the queue; the next check cancels it there.
        let cancelled = AtomicBool::new(false);
        assert!(coordinator
            .acquire(|| cancelled.swap(true, Ordering::SeqCst))
            .is_none());
        coordinator.answer(first.id(), 'y');
        assert_eq!(reply.try_recv().unwrap(), 'y');
        assert!(coordinator.state.lock().unwrap().queued.is_empty());
        drop(first);
        assert!(coordinator.acquire(|| false).is_some());
    }

    #[test]
    fn reopen_cannot_revive_the_old_owner_or_skip_its_cleanup() {
        let coordinator = Arc::new(Coordinator::default());
        let first = coordinator.acquire(|| false).unwrap();
        let first_id = first.id();
        let reply = first.publish().unwrap();
        coordinator.close();
        coordinator.reopen();
        assert!(!first.is_live());
        assert!(first.publish().is_none());
        assert!(reply.recv_timeout(Duration::from_secs(2)).is_err());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let other = coordinator.clone();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let second = other
                .acquire(|| std::time::Instant::now() >= deadline)
                .unwrap();
            let reply = second.publish().unwrap();
            other.answer(first_id, 'y');
            assert!(reply.try_recv().is_err());
            other.answer(second.id(), 'n');
            assert_eq!(reply.recv_timeout(Duration::from_secs(2)).unwrap(), 'n');
            drop(second);
            done_tx.send(()).unwrap();
        });
        wait_for_queued(&coordinator, 1);
        assert_eq!(coordinator.state.lock().unwrap().owner, Some(first_id));
        drop(first);
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn shutdown_disconnects_the_prompt_and_refuses_new_requests() {
        let coordinator = Coordinator::default();
        let first = coordinator.acquire(|| false).unwrap();
        let reply = first.publish().unwrap();
        coordinator.close();
        assert!(reply.recv_timeout(Duration::from_secs(2)).is_err());
        assert!(coordinator.acquire(|| false).is_none());
        coordinator.answer(first.id(), 'y');
        drop(first);
        coordinator.reopen();
        assert!(coordinator.acquire(|| false).is_some());
    }
}
