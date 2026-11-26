//! Shared rooms publish only the user prompt and the selected assistant's reply.
//! Private sessions, tool payloads and working state stay outside the room transcript.
use super::*;
use crate::core::types::Message;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Room {
    pub id: String,
    title: String,
    members: Vec<String>,
    revision: u64,
    created: i64,
}

pub(super) fn directory(key: &str) -> Result<PathBuf> {
    Ok(root().join("rooms").join(id(key)?))
}

fn load(key: &str) -> Result<Room> {
    let row: Room = read(&directory(key)?.join("room.json")).context("conversation unavailable")?;
    ensure!(row.id == key, "mismatched room record");
    Ok(row)
}

pub(super) fn list() -> Result<Vec<Room>> {
    let mut rows = list_ids(&root().join("rooms"), "room.json")?
        .into_iter()
        .map(|key| load(&key))
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    Ok(rows)
}

pub(super) fn save(
    existing: Option<(&str, u64)>,
    title: &str,
    members: Vec<String>,
) -> Result<Room> {
    let title = label(title)?;
    let mut validated = vec![];
    for key in members {
        profile(&key)?;
        if !validated.contains(&key) {
            validated.push(key);
        }
    }
    ensure!(
        (2..=64).contains(&validated.len()),
        "a group requires 2 to 64 different assistants"
    );
    let _lock;
    let (mut room, dir) = if let Some((key, revision)) = existing {
        _lock = RepoTxnLock::acquire_exclusive(
            &crate::core::workspace_txn::store_lock(
                "assistant_turn",
                &format!("room:{}", id(key)?),
            ),
            Duration::ZERO,
        )
        .context("group is busy")?;
        let room = load(key)?;
        ensure!(
            room.revision == revision,
            "group changed in another window; reload before saving"
        );
        (room, directory(key)?)
    } else {
        let (id, dir) = allocate(&root().join("rooms"))?;
        (
            Room {
                id,
                title: String::new(),
                members: vec![],
                revision: 0,
                created: chrono::Utc::now().timestamp_millis(),
            },
            dir,
        )
    };
    room.revision = room
        .revision
        .checked_add(1)
        .context("group revision exhausted")?;
    room.title = title;
    room.members = validated;
    write(&dir.join("room.json"), &room)?;
    Ok(room)
}

pub(super) fn binding(profile: Profile, key: &str) -> Result<Binding> {
    let room = load(key)?;
    ensure!(
        room.members.contains(&profile.id),
        "assistant is not a member of this group"
    );
    Ok(Binding {
        directory: directory(key)?.join("members").join(&profile.id),
        profile,
        thread: Thread {
            id: key.to_owned(),
            title: room.title.clone(),
            created: room.created,
        },
        room: Some(room),
    })
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RoomTurn {
    assistant: String,
    name: String,
    prompt: String,
    answer: Option<String>,
}

fn turns(room: &Room) -> Result<Vec<PathBuf>> {
    let dir = directory(&room.id)?.join("turns");
    if !dir.try_exists()? {
        return Ok(vec![]);
    }
    let mut files = vec![];
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit()))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_turn(path: &Path) -> Result<RoomTurn> {
    let bytes = session_store::read_bounded(path, 16 * 1024 * 1024)
        .map_err(|e| anyhow::anyhow!("cannot read room turn: {e:?}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn history(room: &Room) -> Result<Vec<Value>> {
    let mut rows = vec![];
    for path in turns(room)? {
        let turn = read_turn(&path)?;
        rows.push(json!({"role":"user","text":turn.prompt}));
        if let Some(text) = turn.answer {
            rows.push(json!({"role":"assistant","text":text,"author":turn.name,"assistant":turn.assistant}));
        }
    }
    Ok(rows)
}

pub(crate) struct Publication {
    path: PathBuf,
    turn: RoomTurn,
}
impl Publication {
    pub(crate) fn finish(mut self, answer: Option<&str>) -> Result<()> {
        self.turn.answer = answer.filter(|s| !s.is_empty()).map(str::to_owned);
        write(&self.path, &self.turn)
    }
}

impl Binding {
    pub(crate) fn in_room(&self) -> bool {
        self.room.is_some()
    }

    /// The room lock is held for the entire turn, including atomic publication.
    pub(crate) fn start_room_turn(
        &self,
        prompt: &str,
        history: &mut Vec<Message>,
    ) -> Result<Option<Publication>> {
        let Some(room) = &self.room else {
            return Ok(None);
        };
        let files = turns(room)?;
        let mut remaining = 6000;
        let mut recent = vec![];
        for file in files.iter().rev().take(12) {
            let turn = read_turn(file)?;
            let Some(answer) = turn.answer else { continue };
            let limit = remaining / 2;
            if limit < 40 {
                break;
            }
            let prompt: String = turn.prompt.chars().take(limit).collect();
            let reply: String = format!("[{}] {}", turn.name, answer)
                .chars()
                .take(limit)
                .collect();
            remaining -= prompt.chars().count() + reply.chars().count();
            recent.push((prompt, reply));
        }
        for (q, a) in recent.into_iter().rev() {
            history.extend([Message::user(&q), Message::assistant(&a)]);
        }
        let next = files
            .last()
            .and_then(|p| p.file_stem()?.to_str()?.parse::<u64>().ok())
            .unwrap_or(0)
            .checked_add(1)
            .context("room sequence exhausted")?;
        let folder = directory(&room.id)?.join("turns");
        fs::create_dir_all(&folder)?;
        let path = folder.join(format!("{next:020}.json"));
        let turn = RoomTurn {
            assistant: self.profile.id.clone(),
            name: self.profile.name.clone(),
            prompt: prompt.into(),
            answer: None,
        };
        write(&path, &turn)?;
        Ok(Some(Publication { path, turn }))
    }
}
