//! Parent-run media accounting. Performance admission is shared by children, never disclosure consent.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct MediaBudget {
    reserved: AtomicUsize,
    peak: AtomicUsize,
    attempts: AtomicUsize,
    json_bytes: AtomicUsize,
    image_uri_bytes: AtomicUsize,
    images: AtomicUsize,
    transform: std::sync::Arc<tokio::sync::Semaphore>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MediaUsage {
    pub reserved: usize,
    pub peak: usize,
    pub attempts: usize,
    pub json_bytes: usize,
    pub image_uri_bytes: usize,
    pub images: usize,
}

pub(crate) struct MediaTransformLease {
    _parent: tokio::sync::OwnedSemaphorePermit,
    _worker: tokio::sync::OwnedSemaphorePermit,
    _memory: crate::llm::image_send::ImageMemoryLease,
}

pub(crate) struct MediaLease {
    budget: Arc<MediaBudget>,
    bytes: usize,
    // Global ownership is retained until every request/body/retry owner has returned.
    _global: crate::llm::image_send::ImageMemoryLease,
}

impl Default for MediaBudget {
    fn default() -> Self {
        Self {
            reserved: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            attempts: AtomicUsize::new(0),
            json_bytes: AtomicUsize::new(0),
            image_uri_bytes: AtomicUsize::new(0),
            images: AtomicUsize::new(0),
            transform: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

impl MediaBudget {
    /// Nonblocking admission before cloning input or queueing CPU work. The returned owner moves
    /// into the blocking worker, so cancellation of its async waiter cannot release permits early.
    pub fn transform_lease(&self, source_bytes: usize) -> Option<MediaTransformLease> {
        static WORKERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        let parent = self.transform.clone().try_acquire_owned().ok()?;
        let worker = WORKERS
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
            .clone()
            .try_acquire_owned()
            .ok()?;
        let memory = crate::llm::image_send::try_image_memory_lease(source_bytes)?;
        Some(MediaTransformLease {
            _parent: parent,
            _worker: worker,
            _memory: memory,
        })
    }
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> anyhow::Result<MediaLease> {
        let global = crate::llm::image_send::try_image_memory_lease(bytes)
            .ok_or_else(|| anyhow::anyhow!("image request memory budget busy or exceeded; reduce selected images or retry after active image work completes"))?;
        let previous = self
            .reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= crate::llm::image_send::IMAGE_MEMORY_BUDGET)
            })
            .map_err(|_| anyhow::anyhow!("parent-run image memory budget exceeded"))?;
        self.peak.fetch_max(previous + bytes, Ordering::Relaxed);
        Ok(MediaLease {
            budget: self.clone(),
            bytes,
            _global: global,
        })
    }

    /// Count an attempted HTTP send, including retries/failures, not a successful-delivery receipt.
    pub fn record_attempt(&self, json_bytes: usize, image_uri_bytes: usize, images: usize) {
        for (counter, value) in [
            (&self.attempts, 1),
            (&self.json_bytes, json_bytes),
            (&self.image_uri_bytes, image_uri_bytes),
            (&self.images, images),
        ] {
            let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                Some(old.saturating_add(value))
            });
        }
    }

    pub fn snapshot(&self) -> MediaUsage {
        MediaUsage {
            reserved: self.reserved.load(Ordering::Acquire),
            peak: self.peak.load(Ordering::Relaxed),
            attempts: self.attempts.load(Ordering::Relaxed),
            json_bytes: self.json_bytes.load(Ordering::Relaxed),
            image_uri_bytes: self.image_uri_bytes.load(Ordering::Relaxed),
            images: self.images.load(Ordering::Relaxed),
        }
    }
}

impl Drop for MediaLease {
    fn drop(&mut self) {
        self.budget.reserved.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transform_admission_is_shared_bounded_and_releases_failed_admission() {
        let _guard = crate::llm::image_send::image_test_lock();
        let parent = Arc::new(MediaBudget::default());
        let child = parent.clone();
        let independent = MediaBudget::default();
        let third = MediaBudget::default();
        let first = parent.transform_lease(1024).unwrap();
        assert!(
            child.transform_lease(1).is_none(),
            "one transform across a parent and all children"
        );
        let second = independent.transform_lease(1024).unwrap();
        assert!(
            third.transform_lease(1).is_none(),
            "only two workers across independent runs"
        );
        drop(first);
        assert!(third.transform_lease(usize::MAX).is_none());
        let after_failure = third
            .transform_lease(1)
            .expect("failed memory admission releases worker/parent permits");
        drop(after_failure);
        drop(second);
        assert!(child.transform_lease(1).is_some());
    }

    #[test]
    fn cancelled_waiter_does_not_release_running_codec_admission() {
        let _guard = crate::llm::image_send::image_test_lock();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let parent = Arc::new(MediaBudget::default());
                let worker_parent = parent.clone();
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
                let waiter = tokio::spawn(async move {
                    let lease = worker_parent.transform_lease(4096).unwrap();
                    tokio::task::spawn_blocking(move || {
                        let _ = started_tx.send(());
                        // A failed assertion/cancelled test cannot strand the blocking worker forever.
                        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
                        drop(lease);
                        let _ = finished_tx.send(());
                    })
                    .await
                    .unwrap();
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
                    .await
                    .unwrap()
                    .unwrap();
                waiter.abort();
                assert!(waiter.await.unwrap_err().is_cancelled());
                assert!(
                    parent.transform_lease(1).is_none(),
                    "codec still owns the episode after async cancellation"
                );
                release_tx.send(()).unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), finished_rx)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(parent.transform_lease(1).is_some());
                assert_eq!(
                    parent.snapshot().attempts,
                    0,
                    "preparation cannot publish an HTTP attempt or delivery"
                );
            });
    }

    #[test]
    fn cloned_parent_accounting_survives_children_retries_and_lease_owners() {
        let _guard = crate::llm::image_send::image_test_lock();
        let parent = Arc::new(MediaBudget::default());
        let child = parent.clone();
        let lease = child.reserve(4096).unwrap();
        parent.record_attempt(2000, 1000, 1);
        child.record_attempt(2000, 1000, 1);
        let usage = parent.snapshot();
        assert_eq!(
            (usage.reserved, usage.peak, usage.attempts),
            (4096, 4096, 2)
        );
        assert_eq!(
            (usage.json_bytes, usage.image_uri_bytes, usage.images),
            (4000, 2000, 2)
        );
        assert!(child.reserve(usize::MAX).is_err());
        drop(child);
        assert_eq!(parent.snapshot().reserved, 4096);
        drop(lease);
        assert_eq!(parent.snapshot().reserved, 0);
        assert_eq!(parent.snapshot().attempts, 2);
        assert_eq!(
            Arc::new(MediaBudget::default()).snapshot(),
            MediaUsage::default()
        );
    }
}
