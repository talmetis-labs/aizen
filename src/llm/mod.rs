//! The provider/LLM layer — the OpenAI-compatible streaming chat client that every
//! agent loop, workflow, and one-shot chat ultimately calls through.

pub mod account;
pub mod client;
pub mod codex_models;
pub mod gateway;
pub mod oauth_codex;
pub mod replay;
pub mod responses_codex;
