use super::Message;
use serde_json::Value;
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

/// The transport address is also the lane key. Plain chats retain their original decimal key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Destination {
    pub(super) chat: i64,
    topic: Topic,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Topic {
    Plain,
    General,
    Forum(NonZeroU64),
    Direct(NonZeroU64),
}

impl From<i64> for Destination {
    fn from(chat: i64) -> Self {
        Self {
            chat,
            topic: Topic::Plain,
        }
    }
}

impl Destination {
    pub(super) fn from_message(message: &Message) -> Option<Self> {
        let topic = match (&message.direct_messages_topic, message.message_thread_id) {
            (Some(direct), None) => Topic::Direct(NonZeroU64::new(direct.topic_id)?),
            (Some(_), Some(_)) => return None,
            (None, _) if message.chat.is_direct_messages => return None,
            (None, Some(id)) => Topic::Forum(NonZeroU64::new(id)?),
            (None, None) if message.is_topic_message => return None,
            (None, None) if message.chat.is_forum => Topic::General,
            (None, None) => Topic::Plain,
        };
        Some(Self {
            chat: message.chat.id,
            topic,
        })
    }

    pub(super) fn legacy_parent(self) -> Option<String> {
        (self.topic != Topic::Plain).then(|| self.chat.to_string())
    }

    pub(super) fn message_params(self, mut body: Value) -> Value {
        body["chat_id"] = self.chat.into();
        match self.topic {
            Topic::Forum(id) => body["message_thread_id"] = id.get().into(),
            Topic::Direct(id) => body["direct_messages_topic_id"] = id.get().into(),
            Topic::General | Topic::Plain => {}
        }
        body
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.chat)?;
        match self.topic {
            Topic::Plain => Ok(()),
            Topic::General => f.write_str("_general"),
            Topic::Forum(id) => write!(f, "_topic_{id}"),
            Topic::Direct(id) => write!(f, "_direct_{id}"),
        }
    }
}

impl FromStr for Destination {
    type Err = &'static str;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (chat, suffix) = raw.split_once('_').unwrap_or((raw, ""));
        let chat = chat.parse().map_err(|_| "invalid Telegram chat")?;
        let topic = match suffix {
            "" => Topic::Plain,
            "general" => Topic::General,
            _ => {
                let (kind, id) = suffix.split_once('_').ok_or("invalid Telegram topic")?;
                let id = id.parse().map_err(|_| "invalid Telegram topic ID")?;
                match kind {
                    "topic" => Topic::Forum(id),
                    "direct" => Topic::Direct(id),
                    _ => return Err("unknown Telegram topic type"),
                }
            }
        };
        let destination = Self { chat, topic };
        if destination.to_string() != raw {
            return Err("noncanonical Telegram destination");
        }
        Ok(destination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbound_addresses_round_trip_to_distinct_sessions_and_reply_parameters() {
        for (fields, key, reply) in [
            (json!({}), "-90", json!({"chat_id":-90})),
            (
                json!({"chat":{"id":-90,"is_forum":true}}),
                "-90_general",
                json!({"chat_id":-90}),
            ),
            (
                json!({"message_thread_id":7}),
                "-90_topic_7",
                json!({"chat_id":-90,"message_thread_id":7}),
            ),
            (
                json!({"message_thread_id":8}),
                "-90_topic_8",
                json!({"chat_id":-90,"message_thread_id":8}),
            ),
            (
                json!({"direct_messages_topic":{"topic_id":7}}),
                "-90_direct_7",
                json!({"chat_id":-90,"direct_messages_topic_id":7}),
            ),
        ] {
            let mut wire = json!({"message_id":91,"chat":{"id":-90},"text":"hello"});
            wire.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let message = serde_json::from_value(wire).unwrap();
            let destination = Destination::from_message(&message).unwrap();
            assert_eq!(destination.to_string(), key);
            assert_eq!(key.parse::<Destination>().unwrap(), destination);
            assert_eq!(destination.message_params(json!({})), reply);
        }
        let incomplete = serde_json::from_value(
            json!({"chat":{"id":-90},"message_id":3,"is_topic_message":true}),
        )
        .unwrap();
        assert!(Destination::from_message(&incomplete).is_none());
    }
}
