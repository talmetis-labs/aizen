//! Opt-in synchronous test attribution. Inclusive categories overlap; never sum them as exclusive
//! wall time. Descriptor bytes count bytes returned by read calls, NOT physical disk traffic.
//! No source paths/content, production code, subprocess propagation, or cached safety verdicts.
use serde::Serialize;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Instant};

#[derive(Clone, Copy)]
pub(crate) enum Event {
    ResourceResolve,
    ResourceVerify,
    AncestorVerify,
    LeafVerify,
    CheckedRead,
    PathMetadata,
    PathIdentityOpen,
    PathCanonicalize,
    ReadDescriptorOpen,
    DescriptorMetadata,
    DescriptorIdentity,
    CanonicalPathLookup,
}

impl Event {
    const ALL: [Self; 12] = [
        Self::ResourceResolve,
        Self::ResourceVerify,
        Self::AncestorVerify,
        Self::LeafVerify,
        Self::CheckedRead,
        Self::PathMetadata,
        Self::PathIdentityOpen,
        Self::PathCanonicalize,
        Self::ReadDescriptorOpen,
        Self::DescriptorMetadata,
        Self::DescriptorIdentity,
        Self::CanonicalPathLookup,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::ResourceResolve => "resource_resolve",
            Self::ResourceVerify => "resource_verify",
            Self::AncestorVerify => "ancestor_verify",
            Self::LeafVerify => "leaf_verify",
            Self::CheckedRead => "checked_descriptor_read",
            Self::PathMetadata => "path_metadata",
            Self::PathIdentityOpen => "path_identity_open",
            Self::PathCanonicalize => "path_canonicalize",
            Self::ReadDescriptorOpen => "read_descriptor_open",
            Self::DescriptorMetadata => "descriptor_metadata",
            Self::DescriptorIdentity => "descriptor_identity",
            Self::CanonicalPathLookup => "canonical_path_lookup",
        }
    }
}

#[derive(Clone, Copy, Default, Serialize)]
pub(crate) struct Measurement {
    calls: u64,
    completed: u64, // Closed spans, including errors; this is NOT a success counter.
    inclusive_us: u64,
    returned_bytes: u64,
}

type Counters = [Measurement; Event::ALL.len()];
thread_local! {
    static ACTIVE: RefCell<Option<Rc<RefCell<Counters>>>> = const { RefCell::new(None) };
}

/// Rc makes both guard and spans thread-bound. Old spans cannot charge a later operation.
pub(crate) struct Session(Rc<RefCell<Counters>>);

#[derive(Serialize)]
pub(crate) struct Report {
    pub(crate) all_spans_closed: bool,
    categories: BTreeMap<&'static str, Measurement>,
}

impl Session {
    pub(crate) fn begin() -> anyhow::Result<Self> {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            anyhow::ensure!(active.is_none(), "IO attribution session already active");
            let counters = Rc::new(RefCell::new([Measurement::default(); Event::ALL.len()]));
            *active = Some(counters.clone());
            Ok(Self(counters))
        })
    }

    pub(crate) fn finish(self) -> Report {
        let counters = self.0.borrow();
        Report {
            all_spans_closed: counters.iter().all(|row| row.calls == row.completed),
            categories: Event::ALL
                .into_iter()
                .map(|event| (event.name(), counters[event as usize]))
                .collect(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, &self.0))
            {
                *active = None;
            }
        });
    }
}

pub(crate) struct Span {
    active: Option<(Rc<RefCell<Counters>>, Instant)>,
    event: Event,
    returned_bytes: u64,
}

impl Span {
    pub(crate) fn begin(event: Event) -> Self {
        let active = ACTIVE.with(|active| active.borrow().clone());
        let active = active.map(|counters| {
            counters.borrow_mut()[event as usize].calls += 1;
            (counters, Instant::now())
        });
        Self {
            active,
            event,
            returned_bytes: 0,
        }
    }

    pub(crate) fn returned_bytes(&mut self, bytes: usize) {
        self.returned_bytes = self.returned_bytes.saturating_add(bytes as u64);
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((counters, started)) = &self.active {
            let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            let mut counters = counters.borrow_mut();
            let row = &mut counters[self.event as usize];
            row.completed += 1;
            row.inclusive_us = row.inclusive_us.saturating_add(elapsed);
            row.returned_bytes = row.returned_bytes.saturating_add(self.returned_bytes);
        }
    }
}

#[test]
fn io_attribution_checked_read_counts_actual_bytes_and_keeps_refusals() -> anyhow::Result<()> {
    let fixture = super::file_placement::tests::TestDir::new();
    let path = fixture.path().join("exact");
    std::fs::write(&path, b"exact")?;
    let session = Session::begin()?;
    assert_eq!(
        super::file_placement::read_existing_regular(&path, 5)?,
        b"exact"
    );
    assert!(super::file_placement::read_existing_regular(&path, 4).is_err());
    let report = session.finish();
    assert!(report.all_spans_closed);
    let reads = &report.categories["checked_descriptor_read"];
    assert_eq!(
        reads.calls, 1,
        "oversize must refuse before descriptor read"
    );
    assert_eq!(reads.returned_bytes, 5);
    assert_eq!(report.categories["resource_resolve"].calls, 2);
    assert!(report.categories["ancestor_verify"].calls > 0);
    assert_eq!(std::fs::read(path)?, b"exact");
    Ok(())
}

#[test]
fn io_attribution_counts_nested_calls_and_bytes_without_double_count_claim() -> anyhow::Result<()> {
    let disabled = Span::begin(Event::CheckedRead);
    assert!(disabled.active.is_none());
    let session = Session::begin()?;
    assert!(Session::begin().is_err());
    {
        let _outer = Span::begin(Event::ResourceVerify);
        let mut inner = Span::begin(Event::CheckedRead);
        inner.returned_bytes(7);
        inner.returned_bytes(11);
    }
    let report = session.finish();
    assert!(report.all_spans_closed);
    assert_eq!(report.categories["checked_descriptor_read"].calls, 1);
    assert_eq!(
        report.categories["checked_descriptor_read"].returned_bytes,
        18
    );
    assert_eq!(report.categories["resource_verify"].calls, 1);
    assert_eq!(report.categories["resource_verify"].returned_bytes, 0);
    assert!(Span::begin(Event::CheckedRead).active.is_none());
    Ok(())
}

#[test]
fn io_attribution_old_spans_cannot_charge_new_session() -> anyhow::Result<()> {
    let first = Session::begin()?;
    let late = Span::begin(Event::CheckedRead);
    assert!(!first.finish().all_spans_closed);
    let second = Session::begin()?;
    drop(late);
    let report = second.finish();
    assert!(report.all_spans_closed);
    assert_eq!(report.categories["checked_descriptor_read"].calls, 0);
    Ok(())
}

#[test]
fn io_attribution_unwind_and_thread_isolation_release_session() -> anyhow::Result<()> {
    let session = Session::begin()?;
    std::thread::spawn(|| {
        assert!(Span::begin(Event::CheckedRead).active.is_none());
    })
    .join()
    .unwrap();
    let refused = (|| -> anyhow::Result<()> {
        let _span = Span::begin(Event::ResourceVerify);
        anyhow::bail!("fixture refusal")
    })();
    assert!(refused.is_err());
    let report = session.finish();
    assert!(report.all_spans_closed);
    assert_eq!(report.categories["resource_verify"].completed, 1);
    let panic = std::panic::catch_unwind(|| {
        let _session = Session::begin().unwrap();
        let _span = Span::begin(Event::AncestorVerify);
        panic!("fixture panic");
    });
    assert!(panic.is_err());
    assert!(Session::begin()?.finish().all_spans_closed);
    Ok(())
}
