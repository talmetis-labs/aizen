// Copyright 2026 Aizen authors. Licensed under Apache-2.0.
//! `aizen codebase …` — explicit host-side provisioning for Core's local dense tier.
//!
//! This is not a Memory provider command. Retrieval itself never downloads weights.

use crate::cli_args::CodebaseCmd;
use crate::core::config;
use anyhow::Result;

pub(crate) async fn run(cmd: CodebaseCmd) -> Result<()> {
    match cmd {
        CodebaseCmd::ModelDownload { name } => {
            let name = name.unwrap_or_else(config::embed_model_name);
            crate::retrieval::model_dl::download(&config::models_dir(), &name).await?;
            Ok(())
        }
    }
}
