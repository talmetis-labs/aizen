//! Request-correlated validation, mirrored by the native bridge. No unchecked provider JSON.
use super::{
    api::{self, Validate},
    rich::*,
};
use std::collections::BTreeSet;
fn require(ok: bool, message: &str) -> api::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(api::MemoryError::invalid(message))
    }
}
pub(super) fn scoped(filter: Option<&Filter>) -> api::Result<()> {
    require(
        filter.is_none_or(|f| f.scope != ViewScope::All),
        "all-scope management is not a scoped browser view",
    )
}
fn snapshot(value: &str, as_of: &str) -> api::Result<()> {
    api::opaque_id(value)?;
    api::text(as_of, 32, false)?;
    require(
        as_of.len() == 10 && as_of.as_bytes()[4] == b'-' && as_of.as_bytes()[7] == b'-',
        "invalid inspection date",
    )
}
fn policy(p: &GraphPolicy) -> api::Result<()> {
    require(
        p.floor.is_finite()
            && (0.0..=1.0).contains(&p.floor)
            && p.half_life_days.is_finite()
            && p.half_life_days > 0.0
            && p.max_edges > 0,
        "invalid graph policy",
    )
}
fn reports(items: &[ReportItem]) -> api::Result<()> {
    require(items.len() <= 100, "too many report items")?;
    for item in items {
        api::text(&item.name, 256, false)?;
        api::text(&item.value, api::MAX_TEXT, true)?;
    }
    Ok(())
}
fn reference(r: &Reference) -> api::Result<()> {
    r.validate()
}
fn within(workspace: &str, anchor: &str) -> bool {
    fn normalize(value: &str) -> String {
        #[cfg(windows)]
        {
            value
                .strip_prefix(r"\\?\")
                .unwrap_or(value)
                .replace('\\', "/")
                .to_ascii_lowercase()
                .trim_end_matches('/')
                .to_owned()
        }
        #[cfg(not(windows))]
        {
            value.trim_end_matches('/').to_owned()
        }
    }
    if !std::path::Path::new(anchor).is_absolute()
        || std::path::Path::new(anchor)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let child = normalize(workspace);
    let parent = normalize(anchor);
    child == parent
        || child
            .strip_prefix(&parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
fn record(r: &Record, filter: &Filter, workspace: &str) -> api::Result<()> {
    reference(&r.reference)?;
    require(
        r.revision > 0 && r.confidence.is_finite() && (0.0..=1.0).contains(&r.confidence),
        "invalid record revision/confidence",
    )?;
    api::text(&r.name, 256, false)?;
    api::text(&r.kind, 64, false)?;
    api::text(&r.content, api::MAX_TEXT, true)?;
    api::text(&r.description, 2048, true)?;
    require(
        matches!(
            (r.reference.partition, r.lifecycle),
            (Partition::Entries, Lifecycle::Active | Lifecycle::Retired)
                | (Partition::Archive, Lifecycle::Archive)
                | (Partition::Pending, Lifecycle::Pending)
                | (Partition::Discarded, Lifecycle::Discarded)
        ),
        "record partition/lifecycle mismatch",
    )?;
    require(
        filter.lifecycle == Lifecycle::All || filter.lifecycle == r.lifecycle,
        "record outside lifecycle filter",
    )?;
    require(
        filter.kind.as_ref().is_none_or(|kind| kind == &r.kind),
        "record outside kind filter",
    )?;
    require(
        matches!(
            r.scope,
            ViewScope::User | ViewScope::Device | ViewScope::Project
        ),
        "invalid record placement",
    )?;
    require(
        filter.scope == ViewScope::Current || filter.scope == r.scope,
        "record outside requested scope",
    )?;
    if r.scope == ViewScope::Project {
        let anchor = r
            .anchor
            .as_deref()
            .ok_or_else(|| api::MemoryError::invalid("project record lacks anchor"))?;
        require(
            within(workspace, anchor),
            "record outside workspace placement",
        )?;
    } else {
        require(r.anchor.is_none(), "non-project record has anchor")?;
    }
    for value in r
        .created
        .iter()
        .chain(r.updated.iter())
        .chain(r.last_used.iter())
    {
        api::text(value, 64, false)?;
    }
    for value in r.device.iter().chain(r.origin_id.iter()) {
        api::text(value, 256, false)?;
    }
    for value in r.superseded_by.iter().chain(r.supersedes.iter()) {
        reference(value)?;
    }
    Ok(())
}
fn page<T: serde::Serialize>(p: &PageResult<T>, q: &Page) -> api::Result<()> {
    snapshot(&p.snapshot, &p.as_of)?;
    require(
        p.items.len() <= q.limit && p.items.len() <= p.total_matching,
        "page exceeds requested bounds",
    )?;
    require(
        p.complete == p.next_cursor.is_none(),
        "page completeness/cursor mismatch",
    )?;
    require(p.complete || !p.items.is_empty(), "nonterminal empty page")?;
    if let Some(cursor) = &p.next_cursor {
        api::text(cursor, 2048, false)?;
        require(!cursor.is_empty(), "empty next cursor")?;
    }
    require(
        q.expected_snapshot
            .as_ref()
            .is_none_or(|s| s == &p.snapshot),
        "inspection snapshot changed",
    )?;
    let bytes =
        serde_json::to_vec(p).map_err(|_| api::MemoryError::invalid("invalid page encoding"))?;
    require(
        bytes.len() <= q.max_bytes,
        "page exceeds requested byte budget",
    )
}
pub(super) fn inspection(result: &Inspection, query: &Inspect, workspace: &str) -> api::Result<()> {
    query.validate()?;
    scoped(query.filter())?;
    match (result, query) {
        (
            Inspection::Schema {
                version,
                inspect,
                manage: _,
            },
            Inspect::Schema,
        ) => require(
            *version == VERSION && *inspect,
            "unsupported inspection version",
        )?,
        (
            Inspection::Summary {
                snapshot: s,
                as_of,
                graph_policy,
                ..
            },
            Inspect::Summary { .. },
        ) => {
            snapshot(s, as_of)?;
            policy(graph_policy)?;
        }
        (Inspection::Records { page: p }, Inspect::Records { filter, page: q }) => {
            page(p, q)?;
            let mut ids = BTreeSet::new();
            for r in &p.items {
                record(r, filter, workspace)?;
                require(ids.insert(&r.reference), "duplicate record reference")?;
            }
        }
        (Inspection::GraphNodes { page: p }, Inspect::GraphNodes { filter, page: q }) => {
            page(p, q)?;
            let mut ids = BTreeSet::new();
            for n in &p.items {
                reference(&n.reference)?;
                api::text(&n.name, 256, false)?;
                api::text(&n.kind, 64, false)?;
                if let Some(date) = &n.last_used {
                    api::text(date, 64, false)?;
                }
                require(
                    n.confidence.is_finite()
                        && (0.0..=1.0).contains(&n.confidence)
                        && matches!(
                            n.scope,
                            ViewScope::User | ViewScope::Device | ViewScope::Project
                        )
                        && (filter.scope == ViewScope::Current || filter.scope == n.scope),
                    "invalid graph node metadata/scope",
                )?;
                require(
                    n.reference.partition == Partition::Entries
                        && matches!(n.lifecycle, Lifecycle::Active | Lifecycle::Retired)
                        && (filter.lifecycle == Lifecycle::All || filter.lifecycle == n.lifecycle)
                        && ids.insert(&n.reference),
                    "invalid or duplicate graph node",
                )?;
            }
        }
        (Inspection::GraphEdges { page: p }, Inspect::GraphEdges { page: q, .. }) => {
            page(p, q)?;
            let mut ids = BTreeSet::new();
            for e in &p.items {
                reference(&e.source)?;
                reference(&e.target)?;
                api::text(&e.last_cofire, 64, false)?;
                let key = if e.source < e.target {
                    (&e.source, &e.target)
                } else {
                    (&e.target, &e.source)
                };
                require(
                    e.source != e.target
                        && e.source.partition == Partition::Entries
                        && e.target.partition == Partition::Entries
                        && e.stored_weight.is_finite()
                        && e.stored_weight > 0.0
                        && e.effective_weight.is_finite()
                        && e.effective_weight >= 0.0
                        && e.effective_weight <= e.stored_weight
                        && ids.insert(key),
                    "invalid or duplicate graph edge",
                )?;
            }
        }
        (
            Inspection::Detail {
                snapshot: s,
                as_of,
                record: r,
            },
            Inspect::Detail {
                filter,
                reference: target,
                expected_snapshot,
            },
        ) => {
            snapshot(s, as_of)?;
            record(r, filter, workspace)?;
            require(
                &r.reference == target && expected_snapshot.as_ref().is_none_or(|v| v == s),
                "detail reference/snapshot mismatch",
            )?;
        }
        (
            Inspection::Stats {
                snapshot: s,
                as_of,
                graph_policy,
                items,
                ..
            },
            Inspect::Stats { .. },
        ) => {
            snapshot(s, as_of)?;
            policy(graph_policy)?;
            reports(items)?;
        }
        (
            Inspection::Profile {
                snapshot: s,
                as_of,
                items,
            },
            Inspect::Profile { .. },
        )
        | (
            Inspection::Diagnostics {
                snapshot: s,
                as_of,
                items,
            },
            Inspect::Diagnostics { .. },
        ) => {
            snapshot(s, as_of)?;
            reports(items)?;
        }
        (
            Inspection::Style {
                snapshot: s,
                as_of,
                text,
            },
            Inspect::Style { .. },
        )
        | (
            Inspection::Frozen {
                snapshot: s,
                as_of,
                text,
            },
            Inspect::Frozen { .. },
        ) => {
            snapshot(s, as_of)?;
            api::text(text, api::MAX_TEXT, true)?;
        }
        (
            Inspection::Outcome {
                operation_id,
                status,
                recovery_required,
                receipt: r,
                error,
            },
            Inspect::Outcome {
                operation_id: expected,
            },
        ) => {
            require(
                operation_id == expected,
                "outcome operation identity mismatch",
            )?;
            if let Some(error) = error {
                api::text(&error.message, 1024, false)?;
            }
            match status {
                OutcomeStatus::Unknown => require(
                    r.is_none() && error.is_none(),
                    "unknown outcome carries result",
                )?,
                OutcomeStatus::Done => require(
                    !recovery_required && (r.is_some() != error.is_some()),
                    "done outcome lacks a unique result",
                )?,
                OutcomeStatus::RecoveryRequired => require(
                    *recovery_required && r.is_none(),
                    "invalid recovery outcome",
                )?,
            }
            if let Some(r) = r {
                receipt(r, operation_id, None)?;
            }
        }
        _ => {
            return Err(api::MemoryError::invalid(
                "inspection result variant mismatch",
            ))
        }
    }
    Ok(())
}
pub(super) fn receipt(
    r: &Receipt,
    operation_id: &str,
    command: Option<&Manage>,
) -> api::Result<()> {
    api::opaque_id(&r.operation_id)?;
    api::opaque_id(&r.snapshot_before)?;
    require(
        r.operation_id == operation_id,
        "management receipt identity mismatch",
    )?;
    if let Some(command) = command {
        require(
            r.snapshot_before == command.expected_snapshot,
            "management receipt snapshot mismatch",
        )?;
    }
    require(
        r.changed.len() <= 100 && r.targets.len() <= 100 && r.pairs.len() <= 50,
        "management receipt collection limit",
    )?;
    let mut ids = BTreeSet::new();
    for target in &r.changed {
        reference(target)?;
        require(ids.insert(target), "duplicate changed target")?;
    }
    for target in r.created.iter().chain(r.archived.iter()) {
        reference(target)?;
    }
    for target in &r.targets {
        target.validate()?;
    }
    for pair in &r.pairs {
        pair.keep.validate()?;
        pair.duplicate.validate()?;
        require(
            pair.keep.reference != pair.duplicate.reference,
            "invalid merge pair",
        )?;
    }
    if let Some(binding) = &r.binding {
        api::opaque_id(binding)?;
    }
    for text in r.system.iter().chain(r.input.iter()) {
        api::text(text, api::MAX_TEXT, true)?;
    }
    reports(&r.items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn filter() -> Filter {
        Filter {
            scope: ViewScope::User,
            lifecycle: Lifecycle::All,
            query: String::new(),
            kind: None,
        }
    }
    fn receipt_fixture() -> Receipt {
        serde_json::from_value(json!({"operation_id":"op","snapshot_before":"viewed","changed":[],"created":null,"archived":null,"items":[],"targets":[],"pairs":[],"binding":null,"system":null,"input":null})).unwrap()
    }
    #[test]
    fn rich_receipts_require_operation_and_viewed_snapshot() {
        let r = receipt_fixture();
        let command = Manage {
            filter: filter(),
            expected_snapshot: "viewed".into(),
            action: Action::Worklist,
        };
        assert!(receipt(&r, "op", Some(&command)).is_ok());
        assert!(receipt(&r, "different", Some(&command)).is_err());
        let mut stale = command;
        stale.expected_snapshot = "different".into();
        assert!(receipt(&r, "op", Some(&stale)).is_err());
    }
    #[test]
    fn rich_inspection_refuses_wrong_variants_stale_pages_and_fake_completion() {
        let query = Inspect::Records {
            filter: filter(),
            page: Page {
                limit: 1,
                max_bytes: 4096,
                cursor: None,
                expected_snapshot: Some("viewed".into()),
            },
        };
        let mut result = Inspection::Records {
            page: PageResult {
                snapshot: "viewed".into(),
                as_of: "2026-11-02".into(),
                items: vec![],
                total_matching: 0,
                complete: true,
                next_cursor: None,
            },
        };
        assert!(inspection(&result, &query, "/workspace").is_ok());
        assert!(inspection(
            &Inspection::Schema {
                version: 1,
                inspect: true,
                manage: true
            },
            &query,
            "/workspace"
        )
        .is_err());
        if let Inspection::Records { page } = &mut result {
            page.snapshot = "changed".into();
        }
        assert!(inspection(&result, &query, "/workspace").is_err());
        if let Inspection::Records { page } = &mut result {
            page.snapshot = "viewed".into();
            page.complete = false;
        }
        assert!(inspection(&result, &query, "/workspace").is_err());
    }
    #[test]
    fn rich_outcome_errors_are_explicit_and_operation_correlated() {
        let query = Inspect::Outcome {
            operation_id: "op".into(),
        };
        let result = Inspection::Outcome {
            operation_id: "op".into(),
            status: OutcomeStatus::Done,
            recovery_required: false,
            receipt: None,
            error: Some(api::MemoryError::invalid("rejected")),
        };
        assert!(inspection(&result, &query, "/workspace").is_ok());
        let invalid = Inspection::Outcome {
            operation_id: "op".into(),
            status: OutcomeStatus::Done,
            recovery_required: false,
            receipt: None,
            error: None,
        };
        assert!(inspection(&invalid, &query, "/workspace").is_err());
        assert!(inspection(
            &result,
            &Inspect::Outcome {
                operation_id: "foreign".into()
            },
            "/workspace"
        )
        .is_err());
    }
    #[test]
    fn rich_scope_is_segment_safe_and_never_silently_all() {
        let path = std::env::temp_dir().join("rich-project");
        let workspace = path.join("child");
        assert!(within(workspace.to_str().unwrap(), path.to_str().unwrap()));
        assert!(!within(
            path.with_file_name("rich-project-other").to_str().unwrap(),
            path.to_str().unwrap()
        ));
        let mut broad = filter();
        broad.scope = ViewScope::All;
        assert!(scoped(Some(&broad)).is_err());
    }
}
