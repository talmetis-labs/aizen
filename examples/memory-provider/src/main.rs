//! Small independent provider: JSON-RPC transport and a versioned file-backed records example.
//! It intentionally implements no Mnemos learning, graph, ranking or migration algorithms.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_STATE: u64 = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 1000;
const CAPABILITIES: &[&str] = &["records", "recall", "context"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    id: String,
    content: String,
    scope: String,
    workspace_id: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, Value>,
    revision: u64,
    #[serde(default)]
    archived: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema_version: u32,
    records: BTreeMap<String, Record>,
    receipts: BTreeMap<String, Receipt>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    request: Value,
    result: Value,
}

struct Provider {
    context: Value,
    data_root: PathBuf,
}

struct StoreLock {
    path: PathBuf,
    file: Option<fs::File>,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Windows cannot remove an open file without delete sharing.
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn bounded_string<'a>(value: &'a Value, key: &str, max: usize) -> Result<&'a str, String> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))?;
    if text.is_empty() || text.len() > max || text.chars().any(char::is_control) {
        return Err(format!("invalid {key}"));
    }
    Ok(text)
}

fn valid_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn plain_path(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors() {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err("cannot inspect data path".into()),
        };
        if metadata.file_type().is_symlink() {
            return Err("data path contains a link".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("data path contains a reparse point".into());
            }
        }
    }
    Ok(())
}

impl Provider {
    fn initialize(params: &Value) -> Result<Self, String> {
        if params.pointer("/protocol/major").and_then(Value::as_u64) != Some(1) {
            return Err("incompatible_protocol".into());
        }
        let context = params.get("context").cloned().ok_or("missing context")?;
        bounded_string(&context, "workspace_id", 4096)?;
        let data_root = PathBuf::from(bounded_string(&context, "data_root", 32768)?);
        if !data_root.is_absolute() {
            return Err("data_root must be absolute".into());
        }
        plain_path(&data_root)?;
        Ok(Self { context, data_root })
    }

    fn visible(&self, record: &Record) -> bool {
        !record.archived
            && (record.scope == "user"
                || record.workspace_id.as_deref()
                    == self.context.get("workspace_id").and_then(Value::as_str))
    }

    fn read(&self) -> Result<State, String> {
        let path = self.data_root.join("records.json");
        plain_path(&path)?;
        // A leftover transaction is an explicit recovery condition, never an excuse to overwrite.
        if self.data_root.join("records.next").exists()
            || self.data_root.join("records.previous").exists()
        {
            return Err("recovery_required".into());
        }
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(State {
                    schema_version: 1,
                    ..State::default()
                });
            }
            Err(_) => return Err("cannot inspect records".into()),
        };
        if !metadata.is_file() || metadata.len() > MAX_STATE {
            return Err("invalid records file".into());
        }
        let bytes = fs::read(path).map_err(|_| "cannot read records")?;
        let value = strict_json(&bytes).map_err(|_| "invalid records JSON")?;
        let state: State = serde_json::from_value(value).map_err(|_| "invalid records schema")?;
        if state.schema_version != 1
            || state.records.len() > MAX_RECORDS
            || state.receipts.len() > 4000
        {
            return Err("unsupported or oversized records".into());
        }
        Ok(state)
    }

    fn commit(&self, state: &State) -> Result<(), String> {
        let data = serde_json::to_vec(state).map_err(|_| "cannot serialize records")?;
        if data.len() as u64 > MAX_STATE {
            return Err("records capacity reached".into());
        }
        let next = self.data_root.join("records.next");
        let current = self.data_root.join("records.json");
        let previous = self.data_root.join("records.previous");
        plain_path(&next)?;
        plain_path(&current)?;
        plain_path(&previous)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&next)
            .map_err(|_| "cannot stage records; inspect recovery state")?;
        file.write_all(&data)
            .and_then(|_| file.sync_all())
            .map_err(|_| "cannot persist staged records")?;
        drop(file);
        if current.exists() {
            fs::rename(&current, &previous).map_err(|_| "recovery_required")?;
        }
        fs::rename(&next, &current).map_err(|_| "recovery_required")?;
        if previous.exists() {
            fs::remove_file(previous).map_err(|_| "recovery_required")?;
        }
        Ok(())
    }

    fn mutation(&self, method: &str, params: &Value) -> Result<Value, String> {
        let operation_id = bounded_string(params, "operation_id", 96)?;
        if !valid_id(operation_id) {
            return Err("invalid operation_id".into());
        }
        plain_path(&self.data_root)?;
        fs::create_dir_all(&self.data_root).map_err(|_| "cannot create data directory")?;
        let lock = self.data_root.join("transaction.lock");
        plain_path(&lock)?;
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock)
            .map_err(|_| "busy or recovery_required: transaction.lock exists")?;
        let _guard = StoreLock {
            path: lock,
            file: Some(file),
        };
        let mut state = self.read()?;
        let request = json!({ "method": method, "params": params,
            "workspace_id": self.context.get("workspace_id") });
        if let Some(receipt) = state.receipts.get(operation_id) {
            return if receipt.request == request {
                Ok(receipt.result.clone())
            } else {
                Err("conflict: operation_id reused".into())
            };
        }
        if state.receipts.len() >= 4000 {
            return Err("receipt capacity reached; export data before maintenance".into());
        }
        let result = match method {
            "store" => {
                if state.records.len() >= MAX_RECORDS {
                    return Err("record capacity reached".into());
                }
                let id = format!("example-{operation_id}");
                if state.records.contains_key(&id) {
                    return Err("conflict".into());
                }
                let content = bounded_string(params, "content", 16384)?.to_owned();
                let scope = params
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("project");
                if !matches!(scope, "user" | "project") {
                    return Err("unsupported scope".into());
                }
                let record = Record {
                    id: id.clone(),
                    content,
                    scope: scope.to_owned(),
                    workspace_id: if scope == "project" {
                        self.context
                            .get("workspace_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    } else {
                        None
                    },
                    metadata: BTreeMap::new(),
                    revision: 1,
                    archived: false,
                };
                let value = json!({ "record": record });
                state.records.insert(id, record);
                value
            }
            "update" | "forget" => {
                let id = bounded_string(params, "id", 128)?;
                let record = state.records.get_mut(id).ok_or("not_found")?;
                if !self.visible(record) {
                    return Err("not_found".into());
                }
                let expected = params
                    .get("expected_revision")
                    .and_then(Value::as_u64)
                    .ok_or("missing expected_revision")?;
                if expected != record.revision {
                    return Err("conflict: revision changed".into());
                }
                if method == "forget" {
                    record.archived = true;
                } else {
                    record.content = bounded_string(params, "content", 16384)?.to_owned();
                }
                record.revision = record.revision.checked_add(1).ok_or("revision exhausted")?;
                json!({ "record": record })
            }
            _ => return Err("unsupported_capability".into()),
        };
        state.receipts.insert(
            operation_id.to_owned(),
            Receipt {
                request,
                result: result.clone(),
            },
        );
        self.commit(&state)?;
        Ok(result)
    }

    fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
        match method {
            "capabilities" => Ok(json!({ "capabilities": CAPABILITIES })),
            "health" => Ok(
                json!({ "status": "ready", "provider": "example-memory", "persistent": true,
                    "configuration": { "settings": self.context["settings"], "budgets": self.context["budgets"] } }),
            ),
            "session_start" | "session_end" | "session_reset" => {
                Ok(json!({ "accepted": true, "stateless_session": true }))
            }
            "build_context" => Ok(json!({ "items": [] })),
            "store" | "update" | "forget" => self.mutation(method, params),
            "get" => {
                let state = self.read()?;
                let id = bounded_string(params, "id", 128)?;
                let record = state
                    .records
                    .get(id)
                    .filter(|record| self.visible(record))
                    .ok_or("not_found")?;
                Ok(json!({ "record": record }))
            }
            "recall" | "search" | "list" => {
                let state = self.read()?;
                let query = params.get("query").and_then(Value::as_str).unwrap_or("");
                if query.len() > 16384 {
                    return Err("query too long".into());
                }
                let terms: Vec<String> = query
                    .split_whitespace()
                    .map(str::to_lowercase)
                    .take(64)
                    .collect();
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(10)
                    .clamp(1, 50) as usize;
                let mut hits: Vec<(&Record, f64)> = state
                    .records
                    .values()
                    .filter(|record| self.visible(record))
                    .filter_map(|record| {
                        let body = record.content.to_lowercase();
                        let matches = terms
                            .iter()
                            .filter(|term| body.contains(term.as_str()))
                            .count();
                        if method == "list" || terms.is_empty() || matches > 0 {
                            Some((
                                record,
                                if terms.is_empty() {
                                    1.0
                                } else {
                                    matches as f64 / terms.len() as f64
                                },
                            ))
                        } else {
                            None
                        }
                    })
                    .collect();
                hits.sort_by(|(left, ls), (right, rs)| {
                    rs.total_cmp(ls).then(left.id.cmp(&right.id))
                });
                let items: Vec<Value> = hits
                    .into_iter()
                    .take(limit)
                    .map(|(record, score)| json!({ "record": record, "score": score }))
                    .collect();
                Ok(json!({ "items": items }))
            }
            _ => Err("unsupported_capability".into()),
        }
    }
}

// serde_json::Value normally accepts duplicate keys by overwriting. Reject them recursively.
fn strict_json(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    struct Strict(Value);
    impl<'de> Deserialize<'de> for Strict {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Strict;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("bounded JSON without duplicate keys")
                }
                fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Strict, E> {
                    Ok(Strict(json!(value)))
                }
                fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Strict, E> {
                    Ok(Strict(json!(value)))
                }
                fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Strict, E> {
                    Ok(Strict(json!(value)))
                }
                fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Strict, E> {
                    if value.is_finite() {
                        Ok(Strict(json!(value)))
                    } else {
                        Err(E::custom("nonfinite number"))
                    }
                }
                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Strict, E> {
                    Ok(Strict(json!(value)))
                }
                fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Strict, E> {
                    Ok(Strict(json!(value)))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Strict, E> {
                    Ok(Strict(Value::Null))
                }
                fn visit_none<E: serde::de::Error>(self) -> Result<Strict, E> {
                    Ok(Strict(Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Strict, A::Error> {
                    let mut items = Vec::new();
                    while let Some(Strict(value)) = seq.next_element()? {
                        if items.len() >= 4096 {
                            return Err(serde::de::Error::custom("array too large"));
                        }
                        items.push(value);
                    }
                    Ok(Strict(Value::Array(items)))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Strict, A::Error> {
                    let mut object = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if object.len() >= 1024 || object.contains_key(&key) {
                            return Err(serde::de::Error::custom("duplicate or excessive keys"));
                        }
                        object.insert(key, map.next_value::<Strict>()?.0);
                    }
                    Ok(Strict(Value::Object(object)))
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    serde_json::from_slice::<Strict>(bytes).map(|strict| strict.0)
}

fn read_frame<R: BufRead>(input: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame",
                ))
            };
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if frame.len().saturating_add(count) > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let ended = available[count - 1] == b'\n';
        frame.extend_from_slice(&available[..count]);
        input.consume(count);
        if ended {
            return Ok(Some(frame));
        }
    }
}

fn serve() -> Result<(), String> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut provider = None;
    let mut seen = HashSet::new();
    let mut bytes = 0usize;
    for _ in 0..8 {
        let frame = read_frame(&mut input)
            .map_err(|_| "invalid input frame")?
            .ok_or("unexpected EOF")?;
        bytes = bytes.saturating_add(frame.len());
        if bytes > 16 * MAX_FRAME {
            return Err("transaction capacity reached".into());
        }
        let request: Request =
            serde_json::from_value(strict_json(&frame).map_err(|_| "invalid JSON")?)
                .map_err(|_| "invalid request")?;
        if request.jsonrpc != "2.0" || request.id == 0 || !seen.insert(request.id) {
            return Err("invalid request identity".into());
        }
        let shutdown = request.method == "shutdown";
        let response = if request.method == "initialize" {
            if provider.is_some() {
                Err("already initialized".into())
            } else {
                match Provider::initialize(&request.params) {
                    Ok(initialized) => {
                        provider = Some(initialized);
                        Ok(json!({ "protocol": { "major": 1, "minor": 0 },
                            "provider": { "id": "example-memory", "version": "0.1.0" }, "capabilities": CAPABILITIES }))
                    }
                    Err(error) => Err(error),
                }
            }
        } else if let Some(provider) = provider.as_ref() {
            if shutdown {
                Ok(json!({ "stopping": true }))
            } else {
                provider.call(&request.method, &request.params)
            }
        } else {
            Err("initialize required".into())
        };
        let wire = match response {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": request.id, "result": result }),
            Err(message) => {
                let code = if message.starts_with("unsupported_capability") {
                    "unsupported_capability"
                } else if message.starts_with("incompatible_protocol") {
                    "incompatible_protocol"
                } else if message.starts_with("not_found") {
                    "not_found"
                } else if message.starts_with("conflict") {
                    "conflict"
                } else if message.starts_with("busy") {
                    "busy"
                } else if message.starts_with("recovery_required") {
                    "recovery_required"
                } else {
                    "invalid_request"
                };
                json!({ "jsonrpc": "2.0", "id": request.id,
                    "error": { "code": -32000, "message": message,
                        "data": { "code": code, "message": message } } })
            }
        };
        let mut serialized = serde_json::to_vec(&wire).map_err(|_| "cannot serialize response")?;
        serialized.push(b'\n');
        bytes = bytes.saturating_add(serialized.len());
        if serialized.len() > MAX_FRAME || bytes > 16 * MAX_FRAME {
            return Err("response capacity reached".into());
        }
        output
            .write_all(&serialized)
            .and_then(|_| output.flush())
            .map_err(|_| "cannot write response")?;
        if shutdown {
            return Ok(());
        }
    }
    Err("too many requests".into())
}

fn main() {
    if let Err(error) = serve() {
        eprintln!("example-memory: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_are_rejected_at_every_depth() {
        assert!(strict_json(br#"{"a":1,"a":2}"#).is_err());
        assert!(strict_json(br#"{"a":[{"x":1,"x":2}]}"#).is_err());
        assert!(strict_json(br#"{"a":[1,true,null,"x"]}"#).is_ok());
    }

    #[test]
    fn frame_limit_and_eof_are_explicit() {
        let mut truncated = io::Cursor::new(b"{}".as_slice());
        assert!(read_frame(&mut truncated).is_err());
        let mut oversized = io::Cursor::new(vec![b'x'; MAX_FRAME + 1]);
        assert!(read_frame(&mut oversized).is_err());
        let mut valid = io::Cursor::new(b"{}\n".as_slice());
        assert_eq!(read_frame(&mut valid).unwrap().unwrap(), b"{}\n");
        assert!(read_frame(&mut valid).unwrap().is_none());
    }

    #[test]
    fn operations_are_atomic_and_idempotent_in_the_example_store() {
        let root = std::env::temp_dir().join(format!(
            "example-memory-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = Provider::initialize(&json!({ "protocol": { "major": 1, "minor": 0 },
            "context": { "workspace_id": "fixture-project", "data_root": root } }))
        .unwrap();
        assert!(!root.exists());
        let request = json!({ "operation_id": "first", "content": "Use the example provider", "scope": "project" });
        let first = provider.call("store", &request).unwrap();
        assert_eq!(provider.call("store", &request).unwrap(), first);
        assert_eq!(provider.read().unwrap().records.len(), 1);
        let mut changed = request.clone();
        changed["content"] = json!("different");
        assert!(provider
            .call("store", &changed)
            .unwrap_err()
            .contains("conflict"));
        assert_eq!(
            provider
                .call("search", &json!({ "query": "example" }))
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        fs::write(root.join("records.next"), b"interrupted").unwrap();
        assert_eq!(provider.read().err().unwrap(), "recovery_required");
        fs::remove_dir_all(root).unwrap();
    }
}
