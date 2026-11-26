//! The local owner selects the VPS workspace and topics. The relay can only address those IDs.
use crate::assistants::transfer;
use crate::core::{config, persist, repo_lock::RepoTxnLock, session_store, types::UserTurnId};
use crate::llm::gateway;
use anyhow::{bail, ensure, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Subcommand)]
pub(crate) enum RemoteCmd {
    /// Configure this workspace and run the VPS connector as a persistent Linux user service.
    Enable {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        receive: bool,
    },
    /// Run the connector under an existing service supervisor.
    Serve {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        receive: bool,
    },
    /// Machine bridge for a locally bound Desktop topic. Reads a typed JSON request on stdin.
    #[command(hide = true)]
    TopicCommand { assistant: String, thread: String },
    /// Stop the user service and invalidate its local binding.
    Disable,
    /// Publish an existing saved CLI session, or one existing assistant topic.
    Share {
        #[arg(
            long,
            conflicts_with = "assistant",
            required_unless_present = "assistant"
        )]
        session: Option<String>,
        #[arg(long, requires = "thread")]
        assistant: Option<String>,
        #[arg(long, requires = "assistant")]
        thread: Option<String>,
    },
    /// List the VPS hosts paired with the same account as this Desktop.
    Hosts,
    /// List selectable Mnemos records from a single personal topic.
    Memories { assistant: String, thread: String },
    /// Transfer selected personal topics and their installed persona, skills and selected memories.
    Transfer {
        #[arg(long)]
        device: String,
        #[arg(long)]
        lease: String,
        #[arg(long)]
        assistant: String,
        #[arg(long = "thread", required = true)]
        threads: Vec<String>,
        #[arg(long = "memory")]
        memories: Vec<String>,
    },
}
fn uuid() -> Result<String> {
    Ok(UserTurnId::new()?.as_str().into())
}
#[derive(Clone, Deserialize, Serialize)]
struct Target {
    title: String,
    session: Option<String>,
    assistant: Option<String>,
    thread: Option<String>,
}
#[derive(Deserialize, Serialize)]
struct Share {
    workspace: PathBuf,
    device: String,
    lease: String,
    targets: BTreeMap<String, Target>,
}
fn grant_path() -> PathBuf {
    config::aizen_home().join("vps-chat.json")
}
fn read_grant() -> Result<Share> {
    Ok(serde_json::from_slice(
        &std::fs::read(grant_path()).context("enable Aizen Chat in this workspace first")?,
    )?)
}
fn save_grant(s: &Share) -> Result<()> {
    persist::atomic_write_owner_only_checked(&grant_path(), &serde_json::to_vec(s)?)
}
fn state_lock() -> Result<RepoTxnLock> {
    RepoTxnLock::acquire_exclusive(
        &crate::core::workspace_txn::store_lock("vps_chat", "settings"),
        Duration::from_secs(3),
    )
}
fn configure(workspace: &Path, device: &str) -> Result<Share> {
    let _lock = state_lock()?;
    let workspace = workspace.canonicalize()?;
    ensure!(workspace.is_dir(), "choose a project directory");
    if grant_path().try_exists()? {
        let s = read_grant()?;
        ensure!(
            s.workspace == workspace && s.device == device,
            "disable the previous host binding first"
        );
        return Ok(s);
    }
    let s = Share {
        workspace,
        device: device.into(),
        lease: uuid()?,
        targets: BTreeMap::new(),
    };
    save_grant(&s)?;
    Ok(s)
}
struct Relay {
    client: reqwest::Client,
    url: String,
    token: String,
    device: String,
}
impl Relay {
    fn paired() -> Result<Self> {
        let pin = gateway::load_pin()
            .context("pair this VPS first with aizen gateway login --no-browser")?;
        let origin = pin.gateway.trim_end_matches('/');
        let url = reqwest::Url::parse(origin)?;
        ensure!(
            origin == gateway::DEFAULT_GATEWAY
                || (cfg!(debug_assertions)
                    && url.scheme() == "http"
                    && url.host_str() == Some("127.0.0.1")
                    && url.origin().ascii_serialization() == origin),
            "invalid gateway origin"
        );
        let config = crate::core::cli_config::load();
        let (endpoint, token) = match config.provider(&pin.profile) {
            Some(profile) => (profile.base_url.as_str(), profile.api_key.clone()),
            None => (
                config.base_url.as_deref().unwrap_or(""),
                config.api_key.clone().unwrap_or_default(),
            ),
        };
        ensure!(
            endpoint.trim_end_matches('/') == pin.openai_base_url.trim_end_matches('/'),
            "device profile endpoint changed; pair this machine again"
        );
        ensure!(
            gateway::is_device_token(&token) && pin.expired_at == 0 && !pin.device_id.is_empty(),
            "pair this machine again"
        );
        Ok(Self {
            url: format!("{origin}/v1/device/vps"),
            device: pin.device_id,
            token,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }
    async fn send(&self, q: Value) -> Result<Value> {
        let r = self
            .client
            .post(&self.url)
            .header("X-Aizen-Remote-Effort", "1")
            .bearer_auth(&self.token)
            .json(&q)
            .send()
            .await?;
        if r.status() == reqwest::StatusCode::PAYMENT_REQUIRED {
            bail!("Remote control requires a current Aizen Desktop purchase. Sign in with the account that bought Desktop.");
        }
        ensure!(
            r.status().is_success(),
            "Aizen Chat returned HTTP {}",
            r.status()
        );
        Ok(r.json().await?)
    }
}
fn session_path(slug: &str) -> Result<PathBuf> {
    ensure!(
        !slug.is_empty()
            && slug.len() <= 180
            && session_store::sanitize_name(slug) == slug
            && session_store::session_save_name_error(slug).is_none(),
        "invalid saved session"
    );
    Ok(session_store::sessions_dir().join(format!("{slug}.json")))
}
pub(crate) fn share_session(
    session: Option<String>,
    assistant: Option<String>,
    thread: Option<String>,
) -> Result<String> {
    let _lock = state_lock()?;
    let mut s = read_grant()?;
    ensure!(
        s.workspace == std::env::current_dir()?.canonicalize()?,
        "run share from the configured project"
    );
    let (key, title) = if let Some(slug) = &session {
        let (_, meta, ..) = session_store::read_session_path_with_state(&session_path(slug)?)
            .map_err(anyhow::Error::msg)?;
        ensure!(
            meta.and_then(|m| m.project_root)
                .and_then(|p| Path::new(&p).canonicalize().ok())
                .as_deref()
                == Some(&s.workspace),
            "saved session belongs to a different workspace"
        );
        (format!("cli/{slug}"), slug.clone())
    } else {
        let bot = assistant.as_deref().context("missing assistant")?;
        let topic = thread.as_deref().context("missing thread")?;
        (
            format!("{bot}/{topic}"),
            transfer::topic_title(bot, topic, &s.workspace)?,
        )
    };
    ensure!(
        s.targets.len() < 128 || s.targets.contains_key(&key),
        "too many shared topics"
    );
    s.targets.insert(
        key.clone(),
        Target {
            title,
            session,
            assistant,
            thread,
        },
    );
    save_grant(&s)?;
    Ok(key)
}
fn unit_quote(s: &str) -> Result<String> {
    ensure!(!s.chars().any(char::is_control), "invalid service path");
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
fn service(enable: bool, root: &Path, receive: bool) -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "the background VPS service requires Linux"
    );
    let ctl = |args: &[&str]| -> Result<()> {
        ensure!(
            std::process::Command::new("systemctl")
                .arg("--user")
                .args(args)
                .status()?
                .success(),
            "systemd user service unavailable"
        );
        Ok(())
    };
    if !enable {
        ctl(&["disable", "--now", "aizen-chat.service"])?;
        let _lock = state_lock()?;
        if grant_path().exists() {
            std::fs::remove_file(grant_path())?
        }
        return Ok(());
    }
    let home = std::env::var_os("HOME").context("missing user home")?;
    let file = PathBuf::from(home).join(".config/systemd/user/aizen-chat.service");
    let exe = unit_quote(
        std::env::current_exe()?
            .to_str()
            .context("invalid executable path")?,
    )?;
    let work = unit_quote(root.to_str().context("invalid project path")?)?;
    let flag = if receive { " --receive" } else { "" };
    let unit=format!("[Unit]\nDescription=Aizen Chat VPS\nAfter=network-online.target\n[Service]\nType=simple\nExecStart={exe} remote serve --workspace {work}{flag}\nRestart=on-failure\nRestartSec=5\nUMask=0077\n[Install]\nWantedBy=default.target\n");
    persist::atomic_write_owner_only(&file, unit.as_bytes())?;
    ctl(&["daemon-reload"])?;
    ctl(&["enable", "aizen-chat.service"])?;
    ctl(&["restart", "aizen-chat.service"])?;
    let linger = std::process::Command::new("loginctl")
        .arg("enable-linger")
        .status()?;
    ensure!(
        linger.success(),
        "service started; loginctl enable-linger is required to keep it after the last SSH logout"
    );
    Ok(())
}
pub(crate) async fn run(cmd: RemoteCmd) -> Result<()> {
    let output = match cmd {
        RemoteCmd::TopicCommand { assistant, thread } => {
            let root = std::env::current_dir()?.canonicalize()?;
            let path = transfer::topic_path(&assistant, &thread, &root)?;
            let input: Value =
                serde_json::from_reader(std::io::Read::take(std::io::stdin(), 65537))?;
            if input["menu"] == true {
                crate::remote_slash::menu(&root, input["after"].as_str().unwrap_or(""))
            } else {
                crate::remote_slash::execute(
                    &root,
                    &path,
                    Some((&assistant, &thread)),
                    input["name"].as_str().context("missing command")?,
                    input["args"].as_str().unwrap_or(""),
                )
                .await?
            }
        }
        RemoteCmd::Memories { assistant, thread } => {
            transfer::inventory(&assistant, &thread).await?
        }
        RemoteCmd::Hosts => Relay::paired()?.send(json!({"action":"list"})).await?,
        RemoteCmd::Transfer {
            device,
            lease,
            assistant,
            threads,
            memories,
        } => {
            let r = Relay::paired()?;
            r.send(json!({"action":"access"})).await?;
            let bundle = transfer::export(&assistant, &threads, &memories).await?;
            let job = uuid()?;
            r.send(json!({"action":"submit","kind":"transfer","job":job,"device":device,"lease":lease,"payload":bundle})).await?;
            let mut done = None;
            for _ in 0..180 {
                let v=r.send(json!({"action":"status","kind":"transfer","job":job,"device":device,"lease":lease})).await?;
                match v["state"].as_str() {
                    Some("done") => {
                        done = Some(v["result"].clone());
                        break;
                    }
                    Some("failed") => bail!(
                        "VPS: {}",
                        v["result"]["error"].as_str().unwrap_or("transfer failed")
                    ),
                    _ => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
            done.context("VPS has not confirmed the transfer yet")?
        }
        RemoteCmd::Share {
            session,
            assistant,
            thread,
        } => json!({"topic":share_session(session,assistant,thread)?}),
        RemoteCmd::Enable { workspace, receive } => {
            if gateway::load_pin().is_none() {
                ensure!(
                    std::process::Command::new(std::env::current_exe()?)
                        .args(["gateway", "login", "--no-browser"])
                        .status()?
                        .success(),
                    "pairing cancelled"
                )
            }
            let r = Relay::paired()?;
            r.send(json!({"action":"access"})).await?;
            let s = configure(&workspace, &r.device)?;
            service(true, &s.workspace, receive)?;
            json!({"workspace":s.workspace,"enabled":true})
        }
        RemoteCmd::Disable => {
            service(false, Path::new("."), false)?;
            json!({"enabled":false})
        }
        RemoteCmd::Serve { workspace, receive } => return serve(workspace, receive).await,
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
fn advertised(s: &Share) -> Value {
    json!(s
        .targets
        .iter()
        .map(|(id, t)| json!({"id":id,"title":t.title}))
        .collect::<Vec<_>>())
}
async fn serve(root: PathBuf, receive: bool) -> Result<()> {
    let _single = RepoTxnLock::acquire_exclusive(
        &crate::core::workspace_txn::store_lock("vps_chat", "daemon"),
        Duration::ZERO,
    )
    .context("VPS service already running")?;
    let r = Relay::paired()?;
    let bound = configure(&root, &r.device)?;
    let mut last = Value::Null;
    loop {
        let s = read_grant()?;
        ensure!(
            s.device == r.device && s.lease == bound.lease && s.workspace == bound.workspace,
            "VPS binding changed"
        );
        let topics = advertised(&s);
        let connected = if topics != last {
            r.send(json!({"action":"open","lease":s.lease,"workspace":s.workspace.file_name().unwrap_or_default().to_string_lossy(),"receive":receive,"topics":topics})).await.map(|_|{last=topics})
        } else {
            Ok(())
        };
        let response = match connected {
            Ok(()) => r.send(json!({"action":"poll","lease":s.lease})).await,
            Err(e) => Err(e),
        };
        match response {
            Ok(v) if v["job"].is_object() => {
                let j = &v["job"];
                let id = j["id"].as_str().context("missing job id")?;
                let result = handle(&r, &s, j, receive).await;
                let (failed, data) = match result {
                    Ok(value) => (false, value),
                    Err(e) => (
                        true,
                        json!({"error":format!("{e:#}").chars().take(600).collect::<String>()}),
                    ),
                };
                for _ in 0..5 {
                    if r.send(json!({"action":"finish","lease":s.lease,"job":id,"failed":failed,"result":data})).await.is_ok(){break}
                    tokio::time::sleep(Duration::from_secs(1)).await
                }
            }
            Ok(_) => {}
            Err(_) => {
                last = Value::Null;
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
async fn handle(r: &Relay, s: &Share, job: &Value, receive: bool) -> Result<Value> {
    let kind = job["kind"].as_str().context("missing job kind")?;
    if kind == "transfer" {
        ensure!(receive, "transfers are disabled");
        let pack: transfer::Bundle = serde_json::from_value(job["payload"].clone())?;
        pack.import(&s.workspace).await?;
        let _lock = state_lock()?;
        let mut current = read_grant()?;
        ensure!(current.lease == s.lease, "binding changed");
        let refs = pack.topic_refs();
        for (bot, thread, title) in &refs {
            current.targets.insert(
                format!("{bot}/{thread}"),
                Target {
                    assistant: Some(bot.clone()),
                    thread: Some(thread.clone()),
                    session: None,
                    title: title.clone(),
                },
            );
        }
        save_grant(&current)?;
        return Ok(json!({"topics":refs.len()}));
    }
    let t = s
        .targets
        .get(job["topic"].as_str().unwrap_or(""))
        .context("topic no longer shared")?;
    let path = if let Some(slug) = &t.session {
        session_path(slug)?
    } else {
        transfer::topic_path(
            t.assistant.as_deref().context("missing bot")?,
            t.thread.as_deref().context("missing thread")?,
            &s.workspace,
        )?
    };
    if kind == "history" {
        if let Some(after) = job["payload"]["commands_after"].as_str() {
            return Ok(json!({"commands":crate::remote_slash::menu(&s.workspace,after)}));
        }
        let mut lines = transfer::visible(&path)?;
        let start = lines.len().saturating_sub(100);
        let mut budget = 512000;
        lines = lines.split_off(start);
        lines.reverse();
        lines.retain(|l| {
            if l.content.len() > budget {
                budget = 0;
                false
            } else {
                budget -= l.content.len();
                true
            }
        });
        lines.reverse();
        return Ok(json!({"messages":lines,"commands":crate::remote_slash::menu(&s.workspace,"")}));
    }
    ensure!(kind == "turn", "unsupported work");
    let message = job["payload"]["text"].as_str().context("missing message")?;
    ensure!(
        !message.trim().is_empty() && message.len() <= 24000,
        "invalid message"
    );
    let operation = &job["payload"]["command"];
    let expanded;
    let message = if !operation.is_null() {
        let value = crate::remote_slash::execute(
            &s.workspace,
            &path,
            t.assistant.as_deref().zip(t.thread.as_deref()),
            operation["name"].as_str().context("missing command")?,
            operation["args"].as_str().unwrap_or(""),
        )
        .await?;
        if value.get("answer").is_some() {
            return Ok(value);
        }
        expanded = value["prompt"]
            .as_str()
            .context("invalid command result")?
            .to_owned();
        expanded.as_str()
    } else {
        message
    };
    let mut message = message.to_owned();
    let goal = job["payload"]["goal"].as_bool().unwrap_or(false);
    let effort = job["payload"]["effort"]
        .as_str()
        .filter(|value| !value.is_empty());
    ensure!(
        effort.is_none_or(|value| matches!(value, "low" | "medium" | "high" | "xhigh" | "max")),
        "invalid reasoning effort"
    );
    loop {
        let result = run_shared_turn(r, s, t, &job["id"], &message, goal, effort).await?;
        let Some(mut question) = result.get("ask").cloned() else {
            return Ok(result);
        };
        ensure!(!goal, "unexpected clarification during a goal");
        let id = uuid()?;
        question["id"] = json!(id);
        ensure!(
            question.to_string().len() <= 48_000,
            "clarification too large for web review"
        );
        message = wait_for_clarification(r, s, &job["id"], &question).await?;
    }
}

async fn wait_for_clarification(
    r: &Relay,
    s: &Share,
    job: &Value,
    question: &Value,
) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(600), async {
        let mut missed = 0;
        loop {
            match r
                .send(json!({"action":"progress","lease":s.lease,"job":job,
                "progress":"Waiting for a reply","question":question}))
                .await
            {
                Ok(response) => {
                    missed = 0;
                    if let Some(answer) = response["answer"].as_str() {
                        ensure!(
                            !answer.trim().is_empty() && answer.len() <= 6000,
                            "invalid clarification answer"
                        );
                        return Ok(answer.to_owned());
                    }
                }
                Err(_) => {
                    missed += 1;
                    ensure!(
                        missed < 4,
                        "lost authorization or connection during clarification"
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .context("clarification timed out")?
}

async fn run_shared_turn(
    r: &Relay,
    s: &Share,
    t: &Target,
    job: &Value,
    message: &str,
    goal: bool,
    effort: Option<&str>,
) -> Result<Value> {
    let mut c = tokio::process::Command::new(std::env::current_exe()?);
    c.current_dir(&s.workspace).arg("agent");
    if goal {
        c.arg("--goal");
    }
    if let Some(level) = effort {
        c.args(["--effort", level]);
    }
    let session = t
        .session
        .as_deref()
        .or(t.thread.as_deref())
        .context("missing session")?;
    if let Some(slug) = &t.session {
        c.args(["--resume", slug, "--save-session"]);
    } else {
        c.args([
            "--assistant",
            t.assistant.as_deref().unwrap(),
            "--thread",
            t.thread.as_deref().unwrap(),
        ]);
    }
    struct PrivateTask(PathBuf);
    impl Drop for PrivateTask {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let task = PrivateTask(config::aizen_home().join(format!(".vps-task-{}", uuid()?)));
    persist::atomic_write_owner_only_checked(&task.0, message.as_bytes())?;
    let mut child = c
        .args(["--output-format", "stream-json", "--task-file"])
        .arg(&task.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(child.stdout.take().context("no Core output")?).lines();
    let mut input = child.stdin.take().context("no Core control")?;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut progress = String::from("Running");
    let mut approval: Option<Value> = None;
    let mut result = None;
    let mut missed = 0;
    loop {
        tokio::select! {
           _=tick.tick()=>{
               let v=r.send(json!({"action":"progress","lease":s.lease,"job":job,"progress":progress,"approval":approval})).await;
               let Ok(v)=v else{missed+=1;ensure!(missed<4,"lost authorization or connection during turn");continue};missed=0;
               if let (Some(allow),Some(req))=(v["allow"].as_bool(),approval.as_ref()){
                   let ack=json!({"type":"control_response","response":{"subtype":"success","request_id":req["id"],"response":{"behavior":if allow{"allow"}else{"deny"}}}});
                   input.write_all(format!("{ack}\n").as_bytes()).await?;approval=None;
               }
           },
           line=lines.next_line()=>{let Some(line)=line? else{break};if line.len()>2*1024*1024{bail!("oversized Core event")}
               let Ok(event)=serde_json::from_str::<Value>(&line)else{continue};
               match event["type"].as_str(){
                 Some("control_request")=>{let request=&event["request"];ensure!(request["subtype"]=="can_use_tool","unsupported Core control request");let card=json!({"id":event["request_id"],"tool":request["tool_name"],"input":request["input"],"description":request["description"]});ensure!(card.to_string().len()<47000,"approval too large for web review");approval=Some(card);progress="Waiting for approval".into();tick.reset_immediately();},
                 Some("assistant")=>if let Some(blocks)=event["message"]["content"].as_array(){for b in blocks{if b["type"]=="tool_use"{progress=b["name"].as_str().unwrap_or("Working").chars().take(100).collect()}}},
                 Some("result") if event["parent_tool_use_id"].is_null() => {
                     if event["stop"] == "awaiting_input" {
                         ensure!(event["session"] == session && event["ask"]["questions"].is_array(),
                             "clarification was not saved in the shared session");
                         result = Some(json!({"ask":event["ask"]}));
                     } else {
                         let stop = event["stop"].as_str().unwrap_or("error");
                         ensure!(event["is_error"] != true || stop == "incomplete",
                             "Core stopped: {stop}");
                         result = event["result"].as_str().map(|answer|json!({"answer":answer,"stop":stop}));
                     }
                 },_=>{}
               }
           }
        }
    }
    let exit = child.wait().await?;
    let result = result.context("Core did not return a final answer")?;
    ensure!(
        exit.success() || result["stop"] == "incomplete",
        "Core could not complete the turn"
    );
    Ok(result)
}
