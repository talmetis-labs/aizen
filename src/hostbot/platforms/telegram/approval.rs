use super::{
    destination_for_turn, parse_callback, CallbackQuery, Client, Destination, APPROVAL_TIMEOUT_SECS,
};
use crate::core::exec_ctx::ApprovalRoute;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

struct Prompt {
    destination: Destination,
    message: i64,
    actor: i64,
}

impl Prompt {
    fn matches(&self, callback: &CallbackQuery) -> bool {
        callback.from.id == self.actor
            && callback.message.as_ref().is_some_and(|m| {
                m.message_id == self.message
                    && Destination::from_message(m) == Some(self.destination)
            })
    }
}

struct Pending {
    client: Arc<Client>,
    prompt: Prompt,
    answer: oneshot::Sender<bool>,
}

static PENDING: Lazy<Mutex<HashMap<String, Pending>>> = Lazy::new(Mutex::default);

struct Registration<'a>(&'a str);
impl Drop for Registration<'_> {
    fn drop(&mut self) {
        PENDING.lock().unwrap().remove(self.0);
    }
}

pub(super) fn resolve(id: &str, approved: bool, client: &Arc<Client>, callback: &CallbackQuery) {
    let mut pending = PENDING.lock().unwrap();
    if pending
        .get(id)
        .is_some_and(|p| Arc::ptr_eq(&p.client, client) && p.prompt.matches(callback))
    {
        if let Some(p) = pending.remove(id) {
            let _ = p.answer.send(approved);
        }
    }
}

/// Publish buttons only after the platform has assigned a message ID and its receiver is ready.
pub async fn request_approval_on(text: &str, lane: Option<ApprovalRoute>) -> Option<bool> {
    let (client, destination) = destination_for_turn(lane.as_ref())?;
    let hosted = lane.is_some();
    let actor = match lane {
        Some(lane) => lane.sender_id?.parse::<i64>().ok()?,
        None if destination.chat > 0 => destination.chat,
        None => return Some(false),
    };
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes).ok()?;
    let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let text = format!("🔐 Approve this action?\n\n{text}");
    let msg = client.send_message(destination, &text).await.ok()?;
    let prompt = Prompt {
        destination,
        message: msg.message_id,
        actor,
    };
    let buttons = json!({
        "chat_id": destination.chat, "message_id": msg.message_id,
        "reply_markup": {"inline_keyboard": [[
            {"text":"✅ Approve","callback_data":format!("ng/appr/{id}/y")},
            {"text":"❌ Deny","callback_data":format!("ng/appr/{id}/n")}
        ]]}
    });
    let decision = async {
        if hosted {
            let (answer, rx) = oneshot::channel();
            PENDING.lock().unwrap().insert(
                id.clone(),
                Pending {
                    client: client.clone(),
                    prompt,
                    answer,
                },
            );
            let _registration = Registration(&id);
            client
                .call::<Value>("editMessageReplyMarkup", buttons)
                .await
                .ok()?;
            rx.await.ok()
        } else {
            client
                .call::<Value>("editMessageReplyMarkup", buttons)
                .await
                .ok()?;
            let mut offset = 0;
            loop {
                match client.get_updates(offset, 20).await {
                    Ok(updates) => {
                        for update in updates {
                            offset = offset.max(update.update_id + 1);
                            if let Some(cb) = update.callback_query {
                                if let Some((key, verdict)) =
                                    cb.data.as_deref().and_then(parse_callback)
                                {
                                    if key == id && prompt.matches(&cb) {
                                        let _ = client.answer_callback(&cb.id, "").await;
                                        return Some(verdict);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => tokio::time::sleep(Duration::from_secs(3)).await,
                }
            }
        }
    };
    let approved = tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), decision)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
    let verdict = if approved {
        "✅ Approved"
    } else {
        "❌ Denied / timed out"
    };
    let _ = client
        .edit_text(
            destination.chat,
            msg.message_id,
            &format!("{text}\n\n{verdict}"),
        )
        .await;
    Some(approved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_bound_prompt_and_sender_complete_a_pending_request() {
        let client = Arc::new(Client::new("fixture".into()).unwrap());
        let other = Arc::new(Client::new("other".into()).unwrap());
        let (answer, mut received) = oneshot::channel();
        let id = "bound-test-request";
        let prompt = Prompt {
            destination: "-55_topic_4".parse().unwrap(),
            message: 55,
            actor: 9,
        };
        PENDING.lock().unwrap().insert(
            id.into(),
            Pending {
                client: client.clone(),
                prompt,
                answer,
            },
        );
        let _registration = Registration(id);
        let cb = |topic, actor, message| {
            serde_json::from_value(json!({
                "id":"fixture", "from":{"id":actor}, "message":{
                    "message_id":message, "message_thread_id":topic, "chat":{"id":-55}
                }
            }))
            .unwrap()
        };
        for (origin, callback) in [
            (&other, cb(4, 9, 55)),
            (&client, cb(5, 9, 55)),
            (&client, cb(4, 8, 55)),
            (&client, cb(4, 9, 56)),
        ] {
            resolve(id, true, origin, &callback);
            assert_eq!(
                received.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            );
        }
        resolve(id, true, &client, &cb(4, 9, 55));
        assert_eq!(received.try_recv(), Ok(true));
    }
}
