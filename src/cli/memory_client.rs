//! One bounded stdin request and one stdout response, including protocol errors.
use crate::plugin::client;
use std::io::{Read, Write};

pub(crate) async fn run() -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    let input = std::io::stdin()
        .lock()
        .take((client::MAX_REQUEST + 1) as u64)
        .read_to_end(&mut bytes);
    let response = match input {
        Err(error) => client::failure(String::new(), error.into(), "invalid_request"),
        Ok(_) => match client::parse(&bytes) {
            Ok(request) => client::execute(request).await,
            Err(error) => {
                // Only echo a validated identifier; malformed/duplicate JSON has no trusted ID.
                let id = crate::plugin::memory_api::strict_json(&bytes)
                    .ok()
                    .and_then(|v| {
                        v.get("request_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .filter(|id| crate::plugin::memory_api::opaque_id(id).is_ok())
                    .unwrap_or_default();
                client::failure(id, error, "invalid_request")
            }
        },
    };
    let mut output = serde_json::to_vec(&response)?;
    if output.len() >= client::MAX_RESPONSE {
        let id = serde_json::to_value(&response)?["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        output = serde_json::to_vec(&client::failure(
            id,
            anyhow::anyhow!("Memory client response exceeds 2 MiB; narrow the request"),
            "response_too_large",
        ))?;
    }
    output.push(b'\n');
    std::io::stdout().lock().write_all(&output)?;
    Ok(())
}
