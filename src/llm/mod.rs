//! The provider/LLM layer — the OpenAI-compatible streaming chat client that every
//! agent loop, workflow, and one-shot chat ultimately calls through.

pub mod account;
pub mod client;
pub mod codex_models;
pub(crate) mod delegated;
pub mod gateway;
pub mod image_send;
pub mod media;
pub mod oauth_codex;
pub mod replay;
pub mod responses_codex;
pub(crate) mod stream_watch;
