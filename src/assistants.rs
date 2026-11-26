//! Local assistant profiles and named conversations. One agent CLI process owns one conversation.
pub(crate) mod context;
mod rooms;
pub(crate) mod transfer;
use crate::cli_args::AgentArgs;
use crate::core::{config, persist, repo_lock::RepoTxnLock, session_store, types::UserTurnId};
use anyhow::{ensure, Context, Result};
use clap::{Args, Subcommand};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

#[derive(Debug, Args)]
pub(crate) struct Settings {
    /// Display name. It can change while the assistant ID stays the same.
    #[arg(long)]
    name: String,
    /// Installed persona card. Omit to inherit the configured voice.
    #[arg(long)]
    persona: Option<String>,
    /// Preferred installed skill key. Repeat to select several.
    #[arg(long = "skill")]
    skills: Vec<String>,
    /// Connected platform route, e.g. telegram:default. Empty selects Aizen Desktop.
    #[arg(long)]
    channel: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AssistantCmd {
    List,
    Create {
        #[command(flatten)]
        settings: Settings,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Replace the displayed settings if the previously observed revision still matches.
    Update {
        assistant: String,
        #[arg(long)]
        revision: u64,
        #[command(flatten)]
        settings: Settings,
    },
    /// Available personas and skill keys for this workspace.
    Choices {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    NewThread {
        assistant: String,
        #[arg(long)]
        title: String,
    },
    Threads {
        assistant: String,
    },
    Rooms,
    CreateRoom {
        #[arg(long)]
        title: String,
        #[arg(long = "member", required = true)]
        members: Vec<String>,
    },
    UpdateRoom {
        room: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        title: String,
        #[arg(long = "member", required = true)]
        members: Vec<String>,
    },
    DeleteThread {
        assistant: String,
        thread: String,
    },
    History {
        assistant: String,
        thread: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Profile {
    version: u8,
    id: String,
    revision: u64,
    name: String,
    workspace: PathBuf,
    pub(crate) persona: Option<String>,
    skills: Vec<String>,
    #[serde(default)]
    channel: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Thread {
    id: String,
    title: String,
    created: i64,
}

pub(crate) struct Binding {
    pub(crate) profile: Profile,
    thread: Thread,
    pub(crate) directory: PathBuf,
    room: Option<rooms::Room>,
}

// Each management command or agent turn is a separate OS process. Set before session access.
static ACTIVE: OnceLock<Binding> = OnceLock::new();
pub(crate) fn active() -> Option<&'static Binding> {
    ACTIVE.get()
}

impl Binding {
    pub(crate) fn owner(&self) -> String {
        if self.room.is_some() {
            format!("room:{}:{}", self.thread.id, self.profile.id)
        } else {
            format!("assistant:{}:{}", self.profile.id, self.thread.id)
        }
    }
    pub(crate) fn add_preferences(&self, dynamic: &mut String) {
        if self.room.is_some() {
            dynamic.push_str(&format!("\nYou are the selected group participant {}. Bracketed names on earlier replies identify their speakers.\n", serde_json::to_string(&self.profile.name).unwrap_or_default()));
        }
        if !self.profile.skills.is_empty() {
            dynamic.push_str("\nPreferred local skills for this assistant, when relevant: ");
            dynamic.push_str(&self.profile.skills.join(", "));
            dynamic.push_str(".\n");
        }
    }
    fn bind(self) -> Result<&'static Self> {
        ACTIVE
            .set(self)
            .map_err(|_| anyhow::anyhow!("a conversation is already bound"))?;
        Ok(ACTIVE.get().unwrap())
    }
}

fn root() -> PathBuf {
    config::aizen_home().join("assistants")
}

fn id(value: &str) -> Result<&str> {
    ensure!(
        UserTurnId::parse(value).is_some(),
        "invalid local assistant or thread ID"
    );
    Ok(value)
}

fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = session_store::read_bounded(path, 64 * 1024)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error:?}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid record: {}", path.display()))
}

fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    persist::atomic_write_owner_only(path, &serde_json::to_vec_pretty(value)?)
}

fn profile(key: &str) -> Result<Profile> {
    let row: Profile = read(&root().join(id(key)?).join("profile.json"))?;
    ensure!(
        row.version == 1 && row.id == key,
        "unknown or mismatched assistant profile"
    );
    Ok(row)
}

fn label(s: &str) -> Result<String> {
    let s = s.trim();
    ensure!(
        !s.is_empty() && s.chars().count() <= 120 && !s.chars().any(char::is_control),
        "use 1 to 120 visible characters for the name"
    );
    Ok(s.to_owned())
}

fn workspace(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_dir(),
        "workspace folder is unavailable: {}",
        path.display()
    );
    Ok(std::path::absolute(path)?)
}

fn settings(profile: &mut Profile, form: Settings) -> Result<()> {
    profile.name = label(&form.name)?;
    if let Some(channel) = form.channel {
        if channel.is_empty() {
            profile.channel = None;
        } else {
            let (platform, route) = channel.split_once(':').context("invalid channel")?;
            ensure!(
                matches!(platform, "telegram" | "discord")
                    && !route.is_empty()
                    && route.len() <= 100
                    && route
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')),
                "invalid channel"
            );
            for key in list_ids(&root(), "profile.json")? {
                if key != profile.id {
                    ensure!(
                        self::profile(&key)?.channel.as_deref() != Some(&channel),
                        "channel is already linked to an assistant"
                    );
                }
            }
            profile.channel = Some(channel);
        }
    }
    profile.persona = form
        .persona
        .filter(|s| !s.is_empty())
        .map(|s| {
            crate::persona::load(&s)
                .map(|p| p.name)
                .context("selected persona is unavailable")
        })
        .transpose()?;
    ensure!(form.skills.len() <= 24, "select at most 24 skills");
    if !form.skills.is_empty() {
        std::env::set_current_dir(&profile.workspace)?;
    }
    let mut skills = Vec::new();
    for key in form.skills {
        ensure!(
            !key.is_empty()
                && key.len() <= 120
                && key
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "invalid skill key"
        );
        let skill = crate::skills::load_checked(&key)?
            .with_context(|| format!("skill unavailable in workspace: {key}"))?;
        ensure!(skill.key() == key, "invalid skill key");
        if !skills.contains(&key) {
            skills.push(key);
        }
    }
    profile.skills = skills;
    Ok(())
}

fn allocate(parent: &Path) -> Result<(String, PathBuf)> {
    fs::create_dir_all(parent)?;
    let key = UserTurnId::new()?.as_str().to_owned();
    let dir = parent.join(&key);
    fs::create_dir(&dir)?;
    Ok((key, dir))
}

fn list_ids(dir: &Path, manifest: &str) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry?;
        let key = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir()
            && id(&key).is_ok()
            && entry.path().join(manifest).try_exists()?
        {
            result.push(key);
        }
    }
    Ok(result)
}

fn threads_dir(profile: &Profile) -> PathBuf {
    root().join(&profile.id).join("threads")
}

fn binding(profile: Profile, key: &str) -> Result<Binding> {
    let directory = threads_dir(&profile).join(id(key)?);
    if !directory.join("thread.json").try_exists()? {
        return rooms::binding(profile, key);
    }
    let thread: Thread = read(&directory.join("thread.json"))?;
    ensure!(thread.id == key, "mismatched thread record");
    Ok(Binding {
        profile,
        thread,
        directory,
        room: None,
    })
}

pub(crate) fn bind_run(args: &mut AgentArgs) -> Result<Option<RepoTxnLock>> {
    let Some(assistant) = &args.assistant else {
        return Ok(None);
    };
    let thread = binding(
        profile(assistant)?,
        args.thread.as_deref().context("missing thread")?,
    )?;
    ensure!(
        fs::canonicalize(std::env::current_dir()?)? == fs::canonicalize(&thread.profile.workspace)?,
        "start this assistant from its workspace: {}",
        thread.profile.workspace.display()
    );
    if let Some(name) = &thread.profile.persona {
        ensure!(
            crate::persona::load(name).is_some(),
            "selected persona is unavailable: {name}"
        );
    }
    let lock_key = thread
        .room
        .as_ref()
        .map(|r| format!("room:{}", r.id))
        .unwrap_or_else(|| thread.owner());
    let lock = RepoTxnLock::acquire_exclusive(
        &crate::core::workspace_txn::store_lock("assistant_turn", &lock_key),
        Duration::ZERO,
    )
    .context("cannot start another turn in this conversation")?;
    // Re-read while locked to catch deletion or a membership change after lookup.
    let thread = binding(thread.profile, &thread.thread.id)?.bind()?;
    if thread.room.is_some() {
        args.resume = None;
        args.save_session = false;
        return Ok(Some(lock));
    }
    args.resume = Some(thread.thread.id.clone());
    args.save_session = true;
    Ok(Some(lock))
}

pub(crate) fn manage(cmd: AssistantCmd) -> Result<()> {
    // A connected channel can represent one identity. Serialize creation and relinking across windows.
    let _channel_lock = match &cmd {
        AssistantCmd::Create { settings, .. } | AssistantCmd::Update { settings, .. }
            if settings.channel.is_some() =>
        {
            Some(RepoTxnLock::acquire_exclusive(
                &crate::core::workspace_txn::store_lock("assistant_channel", "directory"),
                Duration::from_secs(3),
            )?)
        }
        _ => None,
    };
    let result = match cmd {
        AssistantCmd::List => {
            let mut rows = list_ids(&root(), "profile.json")?
                .into_iter()
                .map(|key| profile(&key))
                .collect::<Result<Vec<_>>>()?;
            rows.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
            json!(rows)
        }
        AssistantCmd::Create {
            settings: form,
            workspace: selected,
        } => {
            let chosen = selected.as_deref().map(workspace).transpose()?;
            let (id, dir) = allocate(&root())?;
            let work = match chosen {
                Some(path) => path,
                None => {
                    let path = dir.join("workspace");
                    fs::create_dir(&path)?;
                    workspace(&path)?
                }
            };
            let mut row = Profile {
                version: 1,
                id,
                revision: 1,
                name: String::new(),
                persona: None,
                skills: vec![],
                channel: None,
                workspace: work,
            };
            settings(&mut row, form)?;
            write(&dir.join("profile.json"), &row)?;
            json!(row)
        }
        AssistantCmd::Update {
            assistant,
            revision,
            settings: form,
        } => {
            let path = crate::core::workspace_txn::store_lock("assistant_profile", id(&assistant)?);
            let _lock = RepoTxnLock::acquire_exclusive(&path, Duration::from_secs(3))?;
            let mut row = profile(&assistant)?;
            ensure!(
                row.revision == revision,
                "assistant changed in another window; reload before saving"
            );
            settings(&mut row, form)?;
            row.revision = row
                .revision
                .checked_add(1)
                .context("profile revision exhausted")?;
            write(&root().join(&assistant).join("profile.json"), &row)?;
            json!(row)
        }
        AssistantCmd::Choices {
            workspace: selected,
        } => {
            if let Some(path) = selected {
                std::env::set_current_dir(workspace(&path)?)?;
            }
            json!({"personas":crate::persona::list().into_iter().map(|p| p.name).collect::<Vec<_>>(),
                "skills":crate::skills::list().into_iter().map(|s|s.key().to_string()).collect::<Vec<_>>()})
        }
        AssistantCmd::NewThread { assistant, title } => {
            let profile = profile(&assistant)?;
            let title = label(&title)?;
            let (id, directory) = allocate(&threads_dir(&profile))?;
            let thread = Thread {
                id,
                title,
                created: chrono::Utc::now().timestamp_millis(),
            };
            let bound = Binding {
                profile,
                thread,
                directory,
                room: None,
            }
            .bind()?;
            session_store::write_session_with_state(
                &[],
                &bound.thread.id,
                Default::default(),
                None,
                &Default::default(),
            )?;
            write(&bound.directory.join("thread.json"), &bound.thread)?;
            json!(bound.thread)
        }
        AssistantCmd::Threads { assistant } => {
            let p = profile(&assistant)?;
            let mut rows = list_ids(&threads_dir(&p), "thread.json")?
                .into_iter()
                .map(|key| binding(p.clone(), &key).map(|v| v.thread))
                .collect::<Result<Vec<_>>>()?;
            rows.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
            json!(rows)
        }
        AssistantCmd::Rooms => json!(rooms::list()?),
        AssistantCmd::CreateRoom { title, members } => json!(rooms::save(None, &title, members)?),
        AssistantCmd::UpdateRoom {
            room,
            revision,
            title,
            members,
        } => json!(rooms::save(Some((&room, revision)), &title, members)?),
        AssistantCmd::DeleteThread { assistant, thread } => {
            let bound = binding(profile(&assistant)?, &thread)?;
            let (lock_key, directory) = match &bound.room {
                Some(room) => (format!("room:{}", room.id), rooms::directory(&room.id)?),
                None => (bound.owner(), bound.directory),
            };
            let _lock = RepoTxnLock::acquire_exclusive(
                &crate::core::workspace_txn::store_lock("assistant_turn", &lock_key),
                Duration::ZERO,
            )
            .context("conversation is busy")?;
            binding(profile(&assistant)?, &thread)?;
            let trash = root().join(".trash");
            fs::create_dir_all(&trash)?;
            fs::rename(directory, trash.join(UserTurnId::new()?.as_str()))?;
            Value::Null
        }
        AssistantCmd::History { assistant, thread } => {
            let bound = binding(profile(&assistant)?, &thread)?.bind()?;
            if let Some(room) = &bound.room {
                println!("{}", serde_json::to_string(&rooms::history(room)?)?);
                return Ok(());
            }
            let path = session_store::sessions_dir().join(format!("{}.json", bound.thread.id));
            let (mut messages, ..) =
                session_store::read_session_path_with_state(&path).map_err(anyhow::Error::msg)?;
            crate::core::memory_context::strip_history(&mut messages);
            let mut rows: Vec<Value> = vec![];
            for m in messages {
                if matches!(m.role.as_str(), "user" | "assistant") {
                    if let Some(text) = m.content.filter(|s| !s.is_empty()) {
                        rows.push(json!({"role":m.role,"text":text}));
                    }
                }
            }
            json!(rows)
        }
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
