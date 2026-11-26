use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn call(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aizen"))
        .arg("assistant")
        .args(args)
        .current_dir(home)
        .env("AIZEN_HOME", home)
        .env("AIZEN_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run assistant command")
}

fn json(home: &Path, args: &[&str]) -> Value {
    let out = call(home, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn profiles_survive_rename_and_threads_belong_to_one_assistant() {
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce).unwrap();
    let dir = std::env::temp_dir().join(format!("aizen-local-{:032x}", u128::from_le_bytes(nonce)));
    fs::create_dir(&dir).unwrap();
    let p = json(&dir, &["create", "--name", "Desk"]);
    let other = json(&dir, &["create", "--name", "Desk"]);
    let id = p["id"].as_str().unwrap();
    let tid = json(&dir, &["new-thread", id, "--title", "Reports"]);
    let thread = tid["id"].as_str().unwrap();
    assert!(json(&dir, &["history", id, thread])
        .as_array()
        .unwrap()
        .is_empty());
    let renamed = json(
        &dir,
        &["update", id, "--revision", "1", "--name", "Operations"],
    );
    assert_eq!(renamed["id"], id);
    assert_eq!(renamed["revision"], 2);
    assert!(
        !call(&dir, &["update", id, "--revision", "1", "--name", "Old"])
            .status
            .success()
    );
    assert_eq!(json(&dir, &["threads", id])[0]["id"], thread);
    assert!(
        !call(&dir, &["history", other["id"].as_str().unwrap(), thread])
            .status
            .success()
    );
    assert!(!call(&dir, &["threads", "../"]).status.success());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn group_membership_and_topic_removal_are_scoped() {
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce).unwrap();
    let home = std::env::temp_dir().join(format!("aizen-room-{:032x}", u128::from_le_bytes(nonce)));
    fs::create_dir(&home).unwrap();
    let a = json(
        &home,
        &["create", "--name", "An", "--channel", "telegram:default"],
    );
    let b = json(&home, &["create", "--name", "Bi"]);
    let c = json(&home, &["create", "--name", "Chi"]);
    let a = a["id"].as_str().unwrap();
    let b = b["id"].as_str().unwrap();
    let c = c["id"].as_str().unwrap();
    assert!(!call(
        &home,
        &[
            "create",
            "--name",
            "Duplicate",
            "--channel",
            "telegram:default"
        ]
    )
    .status
    .success());
    let room = json(
        &home,
        &[
            "create-room",
            "--title",
            "Product",
            "--member",
            a,
            "--member",
            b,
        ],
    );
    let room = room["id"].as_str().unwrap();
    assert!(!call(&home, &["history", c, room]).status.success());
    assert!(!call(&home, &["delete-thread", c, room]).status.success());
    json(
        &home,
        &[
            "update-room",
            room,
            "--revision",
            "1",
            "--title",
            "Studio",
            "--member",
            b,
            "--member",
            c,
        ],
    );
    assert!(!call(&home, &["history", a, room]).status.success());
    assert!(!call(
        &home,
        &[
            "update-room",
            room,
            "--revision",
            "1",
            "--title",
            "Stale",
            "--member",
            a,
            "--member",
            b
        ]
    )
    .status
    .success());
    let private = json(&home, &["new-thread", b, "--title", "One"]);
    let other = json(&home, &["new-thread", b, "--title", "Two"]);
    json(
        &home,
        &["delete-thread", b, private["id"].as_str().unwrap()],
    );
    assert_eq!(json(&home, &["threads", b])[0]["id"], other["id"]);
    assert_eq!(json(&home, &["rooms"])[0]["title"], "Studio");
    json(&home, &["delete-thread", c, room]);
    assert!(json(&home, &["rooms"]).as_array().unwrap().is_empty());
    assert_eq!(json(&home, &["threads", b]).as_array().unwrap().len(), 1);
    fs::remove_dir_all(home).unwrap();
}
