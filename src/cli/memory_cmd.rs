//! Provider-neutral Memory CLI. Persistent storage/learning is owned by the selected executable.

use crate::cli_args::MemoryCmd;
use crate::plugin::{manager, memory_api as api, runtime};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

pub(crate) async fn run_memory(cmd: MemoryCmd) -> Result<()> {
    if matches!(&cmd, MemoryCmd::Client) {
        return super::memory_client::run().await;
    }
    if let Some(result) = crate::cli::plugin_cmd::memory_control(&cmd) {
        return result;
    }
    let state = manager::read_state()?;
    ensure!(state.enabled, "Memory unavailable or disabled. Install a provider, make it eligible, then explicitly select it with `aizen memory use <id>`; no built-in fallback is used.");
    let (id, _) = manager::selected_at(state.generation)?;
    let installed = manager::info(&id)?;
    let invocation = format!(
        "cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let context = crate::plugin::context::selected(
        &std::env::current_dir()?,
        &invocation,
        &invocation,
        &invocation,
    )?
    .context("Memory was disabled while constructing invocation context")?;
    ensure!(
        context.generation == state.generation,
        "Memory selection changed before dispatch; invocation discarded"
    );
    let cancel = crate::core::cancel::TurnCancel::new();
    let operation_id = |verb: &str| {
        format!(
            "{}-{}",
            verb,
            crate::plugin::manifest::sha256(invocation.as_bytes())
        )
    };
    let (method, params) = match cmd {
        MemoryCmd::Client
        | MemoryCmd::Status { .. }
        | MemoryCmd::Use { .. }
        | MemoryCmd::Enable
        | MemoryCmd::Unselect
        | MemoryCmd::Disable => unreachable!("handled above"),
        MemoryCmd::Add {
            name,
            description,
            mtype,
            body,
        } => {
            let body = match body {
                Some(body) => body,
                None => super::read_stdin("reading Memory body")?,
            };
            ensure!(!body.trim().is_empty(), "empty Memory content");
            (
                api::Method::Store,
                json!({"operation_id":operation_id("store"),"content":body.trim(),"scope":"project",
                "metadata":{"name":name,"description":description,"type":mtype}}),
            )
        }
        MemoryCmd::List {
            scope,
            scope_flag,
            superseded,
        } => (
            api::Method::List,
            json!({"limit":100,"scope":scope.or(scope_flag),"superseded":superseded}),
        ),
        MemoryCmd::Show { id } => (api::Method::Get, json!({"id":id})),
        MemoryCmd::Search {
            query,
            k,
            dimension,
            category,
            scope,
        } => {
            ensure!(k <= 100, "Memory result count exceeds host limit (100)");
            (
                api::Method::Search,
                json!({"query":query,"limit":k,"dimension":dimension,"category":category,"scope":scope}),
            )
        }
        MemoryCmd::Edit {
            id,
            name,
            description,
            mtype,
            body,
            scope,
        } => {
            let previous = call(
                context.clone(),
                api::Method::Get,
                json!({"id":id}),
                cancel.clone(),
            )
            .await?;
            let record: api::RecordResult =
                serde_json::from_value(previous).context("provider returned invalid record")?;
            let content = match body.as_deref() {
                Some("-") => super::read_stdin("reading replacement Memory body")?,
                Some(body) => body.to_owned(),
                None => record.record.content,
            };
            (
                api::Method::Update,
                json!({"operation_id":operation_id("update"),"id":id,"expected_revision":record.record.revision,
                "content":content,"name":name,"description":description,"type":mtype,"scope":scope}),
            )
        }
        MemoryCmd::Forget { id } => {
            let previous = call(
                context.clone(),
                api::Method::Get,
                json!({"id":id}),
                cancel.clone(),
            )
            .await?;
            let record: api::RecordResult = serde_json::from_value(previous)?;
            (
                api::Method::Forget,
                json!({"operation_id":operation_id("forget"),"id":id,"expected_revision":record.record.revision}),
            )
        }
        MemoryCmd::Health => (api::Method::Health, json!({})),
        MemoryCmd::Where => {
            println!(
                "Package: {}\nData: {}\nLegacy data is not adopted automatically.",
                installed.directory.display(),
                manager::data_root(&id)?.display()
            );
            return Ok(());
        }
        MemoryCmd::Learn { text, yes, dry_run } => {
            let text = match text {
                Some(text) => text,
                None => super::read_stdin("reading Memory learning input")?,
            };
            (
                api::Method::ProcessTurn,
                json!({"operation_id":operation_id("learn"),"user_text":text,"transcript":"","confirmed":yes,"dry_run":dry_run}),
            )
        }
        MemoryCmd::Frozen { rebuild } => command(
            "frozen",
            json!({"rebuild":rebuild}),
            &operation_id("frozen"),
        ),
        MemoryCmd::Style => command("style", json!({}), &operation_id("style")),
        MemoryCmd::Profile { json } => {
            command("profile", json!({"json":json}), &operation_id("profile"))
        }
        MemoryCmd::Ask { question, json } => command(
            "ask",
            json!({"question":question,"json":json}),
            &operation_id("ask"),
        ),
        MemoryCmd::Review {
            promote,
            drop_key,
            clear,
        } => command(
            "review",
            json!({"promote":promote,"drop":drop_key,"clear":clear}),
            &operation_id("review"),
        ),
        MemoryCmd::Revive { id } => command("revive", json!({"id":id}), &operation_id("revive")),
        MemoryCmd::AsOf { date } => command("as_of", json!({"date":date}), &operation_id("as-of")),
        MemoryCmd::Supersede { old, new } => command(
            "supersede",
            json!({"old":old,"new":new}),
            &operation_id("supersede"),
        ),
        MemoryCmd::Archive => command("archive", json!({}), &operation_id("archive")),
        MemoryCmd::Restore { id, as_id } => command(
            "restore",
            json!({"id":id,"as_id":as_id}),
            &operation_id("restore"),
        ),
        MemoryCmd::Purge { id, yes } => {
            ensure!(
                yes,
                "purge permanently deletes an archived fact; explicit --yes required"
            );
            command(
                "purge",
                json!({"id":id,"confirmed":true}),
                &operation_id("purge"),
            )
        }
        MemoryCmd::Compact => command("compact", json!({}), &operation_id("compact")),
        MemoryCmd::Consolidate { apply } => command(
            "consolidate",
            json!({"apply":apply}),
            &operation_id("consolidate"),
        ),
        MemoryCmd::Reconcile { apply, reply } => command(
            "reconcile",
            reconcile_arguments(apply, reply.as_deref())?,
            &operation_id("reconcile"),
        ),
        MemoryCmd::Doctor => command("doctor", json!({}), &operation_id("doctor")),
        MemoryCmd::Neighbors { id, k } => command(
            "neighbors",
            json!({"id":id,"limit":k}),
            &operation_id("neighbors"),
        ),
        MemoryCmd::ModelDownload { name } => command(
            "model_download",
            json!({"name":name}),
            &operation_id("model-download"),
        ),
        MemoryCmd::ModelList => command("model_list", json!({}), &operation_id("model-list")),
    };
    // Pins are checked again by the actor while activation admission is held; this earlier snapshot
    // is only input/capability selection, never authority to execute stale package bytes.
    let result = call(context, method, params, cancel).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub(crate) fn reconcile_arguments(apply: bool, path: Option<&std::path::Path>) -> Result<Value> {
    ensure!(
        !apply || path.is_some(),
        "reconcile --apply requires --reply <file>"
    );
    let mut arguments = json!({"apply":apply});
    if let Some(path) = path {
        use std::io::Read;
        #[derive(serde::Deserialize, serde::Serialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            binding_sha256: String,
            completion: String,
        }
        let file = std::fs::File::open(path).context("opening explicit reconcile reply")?;
        ensure!(
            file.metadata()?.is_file(),
            "reconcile reply must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take((64 * 1024 + 1) as u64).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 64 * 1024, "reconcile reply exceeds 64 KiB");
        let reply: Reply = serde_json::from_value(api::strict_json(&bytes)?)
            .context("invalid explicit reconcile reply schema")?;
        ensure!(
            reply.binding_sha256.len() == 64
                && reply
                    .binding_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid reconcile binding digest"
        );
        api::text(&reply.completion, api::MAX_TEXT, true)?;
        ensure!(
            !reply.completion.trim().is_empty(),
            "empty reconcile completion"
        );
        arguments["reply"] = serde_json::to_value(reply)?;
    }
    Ok(arguments)
}

fn command(name: &str, arguments: Value, operation_id: &str) -> (api::Method, Value) {
    (
        api::Method::Command,
        json!({"command":name,"arguments":arguments,"operation_id":operation_id}),
    )
}

async fn call(
    context: api::ProviderContext,
    method: api::Method,
    params: Value,
    cancel: crate::core::cancel::TurnCancel,
) -> Result<Value> {
    runtime::request(
        runtime::ApprovedInvocation::explicit_cli(),
        context,
        method,
        params,
        None,
        cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_reply_is_explicit_bounded_strict_data_not_a_model_route() {
        let fixture = crate::core::file_placement::tests::TestDir::new();
        let path = fixture.path().join("reply.json");
        assert_eq!(
            reconcile_arguments(false, None).unwrap(),
            json!({"apply":false})
        );
        assert!(reconcile_arguments(true, None).is_err());
        let reply = json!({"binding_sha256":"a".repeat(64),"completion":"{\"pairs\":[]}"});
        std::fs::write(&path, serde_json::to_vec(&reply).unwrap()).unwrap();
        assert_eq!(
            reconcile_arguments(true, Some(&path)).unwrap(),
            json!({"apply":true,"reply":reply})
        );
        for invalid in [
            json!({"binding_sha256":"bad","completion":"{}"}),
            json!({"binding_sha256":"a".repeat(64),"completion":""}),
            json!({"binding_sha256":"a".repeat(64),"completion":"{}","admin":true}),
            json!({"binding_sha256":"a".repeat(64),"completion":"x".repeat(api::MAX_TEXT+1)}),
        ] {
            std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(reconcile_arguments(false, Some(&path)).is_err());
        }
        std::fs::write(&path, b"{\"completion\":\"one\",\"completion\":\"two\"}").unwrap();
        assert!(reconcile_arguments(false, Some(&path)).is_err());
        std::fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(reconcile_arguments(false, Some(&path)).is_err());
    }
}
