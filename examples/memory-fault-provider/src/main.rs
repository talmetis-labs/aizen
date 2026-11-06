//! Synthetic protocol faults for Core process/receipt acceptance tests; not a Memory engine.
//! Modes are literal executable filenames selected while packaging owned fixture copies.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn send(id: &Value, value: Value, error: bool) -> io::Result<()> {
    let frame = if error {
        json!({"jsonrpc":"2.0","id":id,"error":{
            "code":-32000,"message":"Synthetic Memory fixture error","data":value}})
    } else {
        json!({"jsonrpc":"2.0","id":id,"result":value})
    };
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &frame)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn record(root: &Path, event: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("events.log"))?;
    writeln!(file, "{event}")?;
    file.sync_all()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let mode = executable
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("missing fixture mode")?;
    if std::env::args().nth(1).as_deref() == Some("--owned-pipe-child") {
        // Deliberately retain inherited pipes beyond direct-parent exit. The Core must settle
        // this owned descendant, not infer that its parent's reply or exit proves tree exit.
        std::thread::sleep(Duration::from_secs(60));
        return Ok(());
    }
    let mut data: Option<PathBuf> = None;
    let input = io::stdin();
    for line in input.lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let id = &request["id"];
        match request["method"].as_str().ok_or("missing method")? {
            "initialize" => {
                let root = PathBuf::from(
                    request["params"]["context"]["data_root"]
                        .as_str()
                        .ok_or("missing owned data root")?,
                );
                if !root.is_absolute() || !root.is_dir() {
                    return Err("fixture data root must be an existing absolute directory".into());
                }
                record(&root, "initialize")?;
                data = Some(root);
                send(
                    id,
                    json!({"protocol":{"major":1,"minor":0},
                    "provider":{"id":"fault-memory","version":"0.1.0"},
                    "capabilities":["records"]}),
                    false,
                )?;
            }
            "store" => {
                let root = data.as_ref().ok_or("uninitialized")?;
                record(root, "store")?;
                let operation = request["params"]["operation_id"]
                    .as_str()
                    .ok_or("missing operation id")?;
                std::fs::write(root.join("operation-id.txt"), operation)?;
                match mode {
                    "conflict" | "pipe-child" => send(
                        id,
                        json!({"code":"conflict","message":"Synthetic no-effect conflict"}),
                        true,
                    )?,
                    "ambiguous" => {
                        record(root, "effect")?;
                        send(
                            id,
                            json!({"code":"ambiguous_result","message":"Synthetic unresolved effect"}),
                            true,
                        )?;
                    }
                    "recovery" => {
                        record(root, "effect")?;
                        send(
                            id,
                            json!({"code":"recovery_required","message":"Synthetic recovery barrier"}),
                            true,
                        )?;
                    }
                    "crash" => {
                        record(root, "effect")?;
                        std::process::exit(17);
                    }
                    "mismatch" => {
                        record(root, "effect")?;
                        send(
                            &json!(id.as_u64().ok_or("invalid id")? + 100),
                            json!({}),
                            false,
                        )?;
                    }
                    "noise" => {
                        record(root, "effect")?;
                        println!("Synthetic non-protocol stdout");
                        io::stdout().flush()?;
                    }
                    "truncated" => {
                        record(root, "effect")?;
                        print!("{{\"jsonrpc\":\"2.0\",\"id\":");
                        io::stdout().flush()?;
                        return Ok(());
                    }
                    "stall" => {
                        record(root, "effect")?;
                        std::thread::sleep(Duration::from_secs(60));
                    }
                    "stderr-flood" => {
                        record(root, "effect")?;
                        let mut stderr = io::stderr().lock();
                        let chunk = [b'x'; 8192];
                        for _ in 0..2176 {
                            stderr.write_all(&chunk)?;
                        }
                        stderr.flush()?;
                    }
                    "oversized" => {
                        record(root, "effect")?;
                        let mut stdout = io::stdout().lock();
                        stdout.write_all(&vec![b'x'; 1024 * 1024 + 1])?;
                        stdout.write_all(b"\n")?;
                        stdout.flush()?;
                    }
                    _ => return Err("unknown literal fixture mode".into()),
                }
            }
            "shutdown" => {
                record(data.as_ref().ok_or("uninitialized")?, "shutdown")?;
                if mode == "pipe-child" {
                    // Explicit owned test binary only; this fixture is not linked into Core.
                    let mut command = Command::new(&executable);
                    command
                        .arg("--owned-pipe-child")
                        .stdin(Stdio::null())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit());
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        command.creation_flags(0x8); // detached, no console helper process
                    }
                    command.spawn()?;
                    record(data.as_ref().ok_or("uninitialized")?, "pipe-child")?;
                }
                send(id, json!({"stopping":true}), false)?;
                return Ok(());
            }
            _ => send(
                id,
                json!({"code":"unsupported_capability","message":"Synthetic fixture method unsupported"}),
                true,
            )?,
        }
    }
    Ok(())
}
