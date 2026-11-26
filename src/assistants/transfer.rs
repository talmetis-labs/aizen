//! Portable, user-selected assistant conversations. Publish the profile last, after provider writes.
use super::*;
use crate::plugin::{context as provider, manager, memory_api as api, runtime};
use crate::skills::{self, SkillLayout};
use anyhow::bail;
use std::collections::{BTreeMap, BTreeSet};

const LIMIT: usize = 4 * 1024 * 1024;
pub(crate) fn topic_title(bot: &str, topic: &str, root: &Path) -> Result<String> {
    let b = binding(profile(bot)?, topic)?;
    ensure!(
        b.room.is_none() && b.profile.workspace.canonicalize()? == root,
        "topic belongs to another workspace"
    );
    Ok(format!("{} · {}", b.profile.name, b.thread.title))
}
pub(crate) fn topic_path(bot: &str, topic: &str, work: &Path) -> Result<PathBuf> {
    topic_title(bot, topic, work)?;
    Ok(root()
        .join(bot)
        .join("threads")
        .join(topic)
        .join("sessions")
        .join(format!("{topic}.json")))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bundle {
    version: u8,
    assistant: String,
    name: String,
    persona: Option<Card>,
    skills: Vec<Asset>,
    topics: Vec<Topic>,
}
#[derive(Serialize, Deserialize)]
struct Card {
    name: String,
    document: String,
}
#[derive(Serialize, Deserialize)]
struct Asset {
    key: String,
    bundle: bool,
    files: BTreeMap<String, Vec<u8>>,
}
#[derive(Serialize, Deserialize)]
struct Topic {
    id: String,
    title: String,
    created: i64,
    messages: Vec<Line>,
    memory: Vec<Fact>,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Line {
    pub role: String,
    pub content: String,
}
#[derive(Serialize, Deserialize)]
struct Fact {
    source: String,
    revision: u64,
    scope: api::Scope,
    content: String,
    metadata: BTreeMap<String, Value>,
}
pub(crate) fn visible(path: &Path) -> Result<Vec<Line>> {
    let (mut messages, ..) =
        session_store::read_session_path_with_state(path).map_err(anyhow::Error::msg)?;
    crate::core::memory_context::strip_history(&mut messages);
    Ok(messages
        .into_iter()
        .filter_map(|m| {
            if !matches!(m.role.as_str(), "user" | "assistant") {
                return None;
            }
            Some(Line {
                role: m.role,
                content: m.content.filter(|s| !s.is_empty())?,
            })
        })
        .collect())
}
fn turn_lock(bot: &str, thread: &str) -> Result<RepoTxnLock> {
    RepoTxnLock::acquire_exclusive(
        &crate::core::workspace_txn::store_lock(
            "assistant_turn",
            &format!("assistant:{bot}:{thread}"),
        ),
        Duration::ZERO,
    )
    .context("topic is busy")
}
async fn memory(
    root: &Path,
    bot: &str,
    thread: &str,
    method: api::Method,
    params: Value,
) -> Result<Value> {
    let op = UserTurnId::new()?;
    let ctx = provider::selected(
        root,
        &format!("assistant:{bot}:{thread}"),
        op.as_str(),
        op.as_str(),
    )?
    .context("Mnemos is disabled")?;
    ensure!(
        manager::selected_at(ctx.generation)?.0 == "mnemos",
        "select Mnemos on this machine first"
    );
    runtime::request(
        runtime::ApprovedInvocation::explicit_cli(),
        ctx,
        method,
        params,
        None,
        crate::core::cancel::TurnCancel::new(),
    )
    .await
}
pub(crate) async fn inventory(bot: &str, thread: &str) -> Result<Value> {
    let p = profile(bot)?;
    let bound = binding(p.clone(), thread)?;
    ensure!(bound.room.is_none(), "select a personal topic");
    let value = memory(
        &p.workspace,
        bot,
        thread,
        api::Method::List,
        json!({"limit":100,"scope":"current"}),
    )
    .await?;
    let hits: api::Hits = serde_json::from_value(value)?;
    Ok(json!(hits.items.into_iter().filter(|h|!h.record.archived && h.record.scope!=api::Scope::Device).map(|h| {
        let r=h.record;json!({"id":r.id,"revision":r.revision,"scope":r.scope,"name":r.metadata.get("name").and_then(Value::as_str).unwrap_or(&r.id)})
    }).collect::<Vec<_>>()))
}
fn safe_relative(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 512
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.starts_with('.')
                && !part.ends_with(['.', ' '])
                && !part.contains(['\\', ':', '\0'])
                && !part.chars().any(char::is_control)
        })
}
fn collect(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, Vec<u8>>,
    used: &mut usize,
) -> Result<()> {
    ensure!(
        out.len() < 256 && dir.strip_prefix(root)?.components().count() < 12,
        "skill resource limit exceeded"
    );
    for entry in fs::read_dir(dir)? {
        let e = entry?;
        let path = e.path();
        let ty = e.file_type()?;
        let name = path
            .strip_prefix(root)?
            .to_str()
            .context("invalid resource path")?
            .replace('\\', "/");
        ensure!(
            safe_relative(&name) && !ty.is_symlink(),
            "unsupported skill resource: {name}"
        );
        if ty.is_dir() {
            collect(root, &path, out, used)?
        } else {
            ensure!(ty.is_file() && out.len() < 256, "invalid skill resource");
            let bytes = session_store::read_bounded(&path, LIMIT.saturating_sub(*used))
                .map_err(|_| anyhow::anyhow!("skill transfer size exceeded"))?;
            *used += bytes.len();
            out.insert(name, bytes);
        }
    }
    Ok(())
}
fn asset(key: &str) -> Result<Asset> {
    let loc = skills::load_checked(key)?
        .and_then(|s| s.location)
        .context("installed skill missing")?;
    let mut files = BTreeMap::new();
    let bundle = loc.layout == SkillLayout::Bundle;
    if bundle {
        collect(&loc.resource_root, &loc.resource_root, &mut files, &mut 0)?
    } else {
        files.insert(
            "SKILL.md".into(),
            session_store::read_bounded(&loc.entry, LIMIT)
                .map_err(|_| anyhow::anyhow!("unreadable skill"))?,
        );
    }
    ensure!(files.contains_key("SKILL.md"), "missing skill entry {key}");
    Ok(Asset {
        key: key.into(),
        bundle,
        files,
    })
}
pub(crate) async fn export(bot: &str, threads: &[String], selected: &[String]) -> Result<Bundle> {
    ensure!(
        !threads.is_empty() && threads.len() <= 64 && selected.len() <= 300,
        "select topics to transfer"
    );
    let p = profile(bot)?;
    std::env::set_current_dir(&p.workspace)?;
    let mut topics = Vec::new();
    let mut consumed = BTreeSet::new();
    for key in threads {
        let _lock = turn_lock(bot, id(key)?)?;
        let bound = binding(p.clone(), key)?;
        ensure!(
            bound.room.is_none(),
            "rooms require their complete membership"
        );
        let path = bound.directory.join("sessions").join(format!("{key}.json"));
        let (_, _, _, task, control) =
            session_store::read_session_path_with_state(&path).map_err(anyhow::Error::msg)?;
        ensure!(
            !control.is_planning() && !task.active,
            "finish or pause the active plan before transferring this topic"
        );
        let mut facts = Vec::new();
        for s in selected {
            let Some(spec) = s.strip_prefix(&format!("{key}/")) else {
                continue;
            };
            let (record, rev) = spec.rsplit_once('@').context("invalid record selection")?;
            ensure!(consumed.insert(s.clone()), "duplicate memory selection");
            let value = memory(
                &p.workspace,
                bot,
                key,
                api::Method::Get,
                json!({"id":record}),
            )
            .await?;
            let r: api::RecordResult = serde_json::from_value(value)?;
            let r = r.record;
            ensure!(
                r.id == record
                    && r.revision == rev.parse::<u64>()?
                    && !r.archived
                    && r.scope != api::Scope::Device,
                "memory changed; refresh selection"
            );
            facts.push(Fact {
                source: r.id,
                revision: r.revision,
                scope: r.scope,
                content: r.content,
                metadata: r.metadata,
            });
        }
        topics.push(Topic {
            id: key.clone(),
            title: bound.thread.title,
            created: bound.thread.created,
            messages: visible(&path)?,
            memory: facts,
        });
    }
    ensure!(
        consumed.len() == selected.len(),
        "memory must belong to a selected topic"
    );
    let persona = p
        .persona
        .map(|name| -> Result<Card> {
            let file = format!("{}.md", skills::sanitize_name(&name));
            let mut path = crate::persona::personas_dir().join(&file);
            if !path.is_file() {
                ensure!(
                    crate::agent::mcp::project_trusted(),
                    "project persona is not trusted"
                );
                path = crate::persona::project_personas_dir().join(file)
            }
            ensure!(crate::persona::load(&name).is_some(), "missing persona");
            Ok(Card {
                name,
                document: fs::read_to_string(path)?,
            })
        })
        .transpose()?;
    let pack = Bundle {
        version: 1,
        assistant: p.id,
        name: p.name,
        persona,
        skills: p.skills.iter().map(|s| asset(s)).collect::<Result<_>>()?,
        topics,
    };
    ensure!(
        serde_json::to_vec(&pack)?.len() <= LIMIT,
        "selection exceeds 4 MiB"
    );
    Ok(pack)
}
fn publish_file(path: &Path, data: &[u8]) -> Result<()> {
    check_ancestors(path)?;
    if path.try_exists()? {
        ensure!(
            path.symlink_metadata()?.is_file() && fs::read(path)? == data,
            "destination conflict: {}",
            path.display()
        );
        return Ok(());
    }
    let parent = path.parent().context("missing parent")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".transfer-{}", UserTurnId::new()?.as_str()));
    persist::atomic_write_owner_only(&tmp, data)?;
    let published = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(tmp);
    published?;
    Ok(())
}
fn check_ancestors(path: &Path) -> Result<()> {
    let home = config::aizen_home().canonicalize()?;
    let relative = path
        .strip_prefix(config::aizen_home())
        .context("invalid destination outside Aizen storage")?;
    let mut parent = home;
    for part in relative.components() {
        let std::path::Component::Normal(part) = part else {
            bail!("invalid transfer path")
        };
        parent.push(part);
        match parent.symlink_metadata() {
            Ok(meta) => ensure!(
                !crate::core::file_placement::is_link_or_reparse(&meta),
                "linked transfer destination"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
impl Bundle {
    pub(crate) fn topic_refs(&self) -> Vec<(String, String, String)> {
        self.topics
            .iter()
            .map(|t| {
                (
                    self.assistant.clone(),
                    t.id.clone(),
                    format!("{} · {}", self.name, t.title),
                )
            })
            .collect()
    }
    pub(crate) async fn import(&self, destination: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= LIMIT
                && self.version == 1
                && !self.topics.is_empty()
                && self.topics.len() <= 64
                && self.skills.len() <= 24,
            "invalid assistant transfer"
        );
        let digest = crate::plugin::manifest::sha256(&bytes);
        let workspace = destination.canonicalize()?;
        std::env::set_current_dir(&workspace)?;
        let bot = id(&self.assistant)?;
        label(&self.name)?;
        let _lock = RepoTxnLock::acquire_exclusive(
            &crate::core::workspace_txn::store_lock("assistant_transfer", bot),
            Duration::from_secs(2),
        )?;
        let target = root().join(bot);
        let receipt = target.join("transfer.sha256");
        check_ancestors(&receipt)?;
        if target.join("profile.json").exists() {
            ensure!(
                fs::read_to_string(&receipt).ok().as_deref() == Some(&digest)
                    && profile(bot)?.workspace == workspace,
                "bot already exists with different data"
            );
            return Ok(());
        }
        let mut seen = BTreeSet::new();
        for t in &self.topics {
            ensure!(
                seen.insert(id(&t.id)?)
                    && label(&t.title).is_ok()
                    && t.messages
                        .iter()
                        .all(|m| matches!(m.role.as_str(), "user" | "assistant")),
                "invalid topic"
            )
        }
        fs::create_dir_all(&target)?;
        publish_file(&receipt, digest.as_bytes())?;
        if let Some(card) = &self.persona {
            label(&card.name)?;
            publish_file(
                &crate::persona::personas_dir()
                    .join(format!("{}.md", skills::sanitize_name(&card.name))),
                card.document.as_bytes(),
            )?
        }
        for a in &self.skills {
            ensure!(
                skills::sanitize_name(&a.key) == a.key
                    && a.files.contains_key("SKILL.md")
                    && a.files.len() <= 256,
                "invalid skill"
            );
            if skills::load_checked(&a.key)?.is_some() {
                let existing = asset(&a.key)?;
                ensure!(
                    existing.bundle == a.bundle && existing.files == a.files,
                    "skill conflict: {}",
                    a.key
                );
                continue;
            }
            if a.bundle {
                for (name, data) in &a.files {
                    ensure!(safe_relative(name), "invalid resource path");
                    publish_file(&skills::project_zone_dir().join(&a.key).join(name), data)?
                }
            } else {
                ensure!(a.files.len() == 1, "invalid flat skill");
                publish_file(
                    &skills::project_zone_dir().join(format!("{}.md", a.key)),
                    &a.files["SKILL.md"],
                )?
            }
            ensure!(
                skills::load_checked(&a.key)?.is_some(),
                "skill could not be loaded"
            );
        }
        for t in &self.topics {
            let dir = target.join("threads").join(&t.id);
            let mut msgs = Vec::new();
            check_ancestors(&dir.join("sessions").join(format!("{}.json", t.id)))?;
            for m in &t.messages {
                msgs.push(if m.role == "user" {
                    crate::core::types::Message::user(&m.content)
                } else {
                    crate::core::types::Message::assistant(&m.content)
                })
            }
            let records = session_store::stored_history(&msgs)?;
            write(
                &dir.join("sessions").join(format!("{}.json", t.id)),
                &json!({"version":2,"messages":records,"meta":{"project_key":config::project_key(),"project_root":workspace,"project_slug":config::project_slug()}}),
            )?;
            write(
                &dir.join("thread.json"),
                &Thread {
                    id: t.id.clone(),
                    title: t.title.clone(),
                    created: t.created,
                },
            )?;
            for f in &t.memory {
                ensure!(
                    f.scope != api::Scope::Device,
                    "device memory cannot be transferred"
                );
                let op = crate::plugin::manifest::sha256(
                    format!("{digest}/{}/{}@{}", t.id, f.source, f.revision).as_bytes(),
                );
                // Mnemos returns additional read-only metadata when retrieving a record.
                // Only the editable fields are valid inputs to its store method.
                let metadata = ["name", "description", "type"]
                    .into_iter()
                    .filter_map(|key| {
                        f.metadata
                            .get(key)
                            .and_then(Value::as_str)
                            .map(|value| (key, value))
                    })
                    .collect::<BTreeMap<_, _>>();
                memory(&workspace,bot,&t.id,api::Method::Store,json!({"operation_id":format!("transfer-{op}"),"scope":f.scope,"content":f.content,"metadata":metadata})).await?;
            }
        }
        let p = Profile {
            version: 1,
            id: bot.into(),
            revision: 1,
            name: self.name.clone(),
            workspace,
            persona: self.persona.as_ref().map(|c| c.name.clone()),
            skills: self.skills.iter().map(|s| s.key.clone()).collect(),
            channel: None,
        };
        write(&target.join("profile.json"), &p)
    }
}
