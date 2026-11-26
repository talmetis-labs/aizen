use super::*;
use crate::core::types::UserTurnId;
use std::{fs, path::PathBuf};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "aizen-context-{}",
            UserTurnId::new().unwrap().as_str()
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("text-context-v1.json"));
        let _ = fs::remove_dir(&self.0);
    }
}
fn exchange(history: &mut Vec<Message>, text: &str) {
    let mut user = Message::user(text);
    user.user_turn_id = Some(UserTurnId::new().unwrap());
    history.extend([user, Message::assistant("Recorded.")]);
}
fn fixture() -> Vec<Message> {
    let mut history = vec![Message::system("Policy")];
    for text in [
        "Original instructions",
        &"decision evidence ".repeat(1100),
        "Recent A",
        "Recent B",
    ] {
        exchange(&mut history, text);
    }
    history
}

#[tokio::test]
async fn cache_is_scoped_to_exact_plain_exchanges_and_preserves_canonical_history() {
    let dir = Directory::new();
    let mut history = fixture();
    let initial = serde_json::to_string(&history).unwrap();
    let first = prepare(&dir.0, &history, |prompt| async move {
        let text = prompt[1].content.as_deref().unwrap();
        assert!(text.contains("decision evidence"));
        assert!(!text.contains("Recent A"));
        Ok("Decision retained.".into())
    })
    .await
    .unwrap();
    assert_eq!(initial, serde_json::to_string(&history).unwrap());
    let reused = prepare(&dir.0, &history, |_| async {
        panic!("unexpected model call")
    })
    .await
    .unwrap();
    let mut outgoing = history.clone();
    reused.apply(&mut outgoing);
    assert_eq!(
        first.savings(&history),
        tokens(&history) - tokens(&outgoing)
    );
    assert_eq!(outgoing[3].role, "user");
    assert_eq!(outgoing[4].role, "assistant");
    assert_eq!(outgoing[4].content.as_deref(), Some("Decision retained."));
    history[3].content.as_mut().unwrap().push_str("correction");
    assert!(
        prepare(&dir.0, &history, |_| async { anyhow::bail!("offline") })
            .await
            .is_none()
    );
    history[3].images.push("data:image/png;base64,AA==".into());
    assert!(
        prepare(&dir.0, &history, |_| async { panic!("image summarized") })
            .await
            .is_none()
    );
    assert_eq!(reused.savings(&history), 0);
}

#[tokio::test]
async fn failed_extension_reuses_only_the_valid_prefix() {
    let dir = Directory::new();
    let mut history = fixture();
    prepare(&dir.0, &history, |_| async {
        Ok("Earlier decision.".into())
    })
    .await
    .unwrap();
    exchange(&mut history, &"later evidence ".repeat(1500));
    exchange(&mut history, "Latest one");
    exchange(&mut history, "Latest two");
    let view = prepare(&dir.0, &history, |prompt| async move {
        let text = prompt[1].content.as_deref().unwrap();
        assert!(text.contains("Earlier decision.") && text.contains("later evidence"));
        assert!(!text.contains("decision evidence"));
        Ok("too long ".repeat(1500))
    })
    .await
    .unwrap();
    view.apply(&mut history);
    assert!(history.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|t| t.contains("later evidence"))));
}
