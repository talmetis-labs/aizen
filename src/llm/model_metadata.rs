//! Pure, tolerant parsing of provider model metadata. No requests or configuration access.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_MODEL_TOKENS: usize = 100_000_000;
const CONTEXT_KEYS: &[&str] = &[
    "context_length",
    "context_window",
    "max_context_length",
    "max_model_len",
];
const INPUT_KEYS: &[&str] = &["max_input_tokens", "input_token_limit"];
const OUTPUT_KEYS: &[&str] = &[
    "max_output_tokens",
    "max_completion_tokens",
    "output_token_limit",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_input_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<usize>,
}

impl ModelLimits {
    /// Output limits do not describe how much input/history the model can accept.
    pub(crate) fn effective_window(self) -> Option<usize> {
        minimum_positive(self.context_window, self.max_input_tokens)
    }

    fn constrained_by(self, other: Self) -> Self {
        Self {
            context_window: minimum_positive(self.context_window, other.context_window),
            max_input_tokens: minimum_positive(self.max_input_tokens, other.max_input_tokens),
            max_output_tokens: minimum_positive(self.max_output_tokens, other.max_output_tokens),
        }
    }
}

fn minimum_positive(left: Option<usize>, right: Option<usize>) -> Option<usize> {
    left.into_iter()
        .chain(right)
        .filter(|value| *value > 0)
        .min()
}

fn token_count(value: &Value) -> Option<usize> {
    let count = match value {
        Value::Number(number) => usize::try_from(number.as_u64()?).ok()?,
        Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            text.parse::<usize>().ok()?
        }
        _ => return None,
    };
    (count > 0).then_some(count.min(MAX_MODEL_TOKENS))
}

fn minimum_field(row: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter()
        .filter_map(|key| row.get(*key).and_then(token_count))
        .min()
}

fn fields(row: &Value) -> ModelLimits {
    ModelLimits {
        context_window: minimum_field(row, CONTEXT_KEYS),
        max_input_tokens: minimum_field(row, INPUT_KEYS),
        max_output_tokens: minimum_field(row, OUTPUT_KEYS),
    }
}

/// Parse only explicit token limits, taking the tighter cap when aliases disagree.
/// `max_tokens` is deliberately ignored: it commonly means a generation default,
/// not a context window. Pricing and free-tier flags are not token metadata.
pub(crate) fn model_limits(row: &Value) -> ModelLimits {
    let mut limits = fields(row);
    if let Some(info) = row.get("model_info") {
        limits = limits.constrained_by(fields(info));
    }
    if let Some(provider) = row.get("top_provider") {
        limits = limits.constrained_by(fields(provider));
    }
    limits
}

/// Select an exact, case-sensitive ID from a model-list envelope or model detail.
/// An identified model with no usable metadata returns `Some(default())`; no
/// matching identity returns `None`. Duplicate IDs can only tighten each cap.
pub(crate) fn selected_model_limits(value: &Value, model: &str) -> Option<ModelLimits> {
    let matches = |row: &Value| row.get("id").and_then(Value::as_str) == Some(model);
    if let Some(data) = value.get("data") {
        if let Some(rows) = data.as_array() {
            return rows.iter().filter(|row| matches(row)).fold(
                None::<ModelLimits>,
                |limits, row| {
                    let parsed = model_limits(row);
                    Some(match limits {
                        Some(previous) => previous.constrained_by(parsed),
                        None => parsed,
                    })
                },
            );
        }
        if matches(data) {
            return Some(model_limits(data));
        }
    }
    matches(value).then(|| model_limits(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_positive_integers_and_ascii_digit_strings() {
        for value in [
            json!(1),
            json!(128_000),
            json!("128000"),
            json!("000128000"),
        ] {
            assert!(token_count(&value).is_some(), "{value}");
        }
        assert_eq!(token_count(&json!(1)), Some(1));
        assert_eq!(token_count(&json!("000128000")), Some(128_000));
        assert_eq!(token_count(&json!(100_000_001)), Some(MAX_MODEL_TOKENS));
        assert_eq!(token_count(&json!("100000001")), Some(MAX_MODEL_TOKENS));
    }

    #[test]
    fn rejects_malformed_zero_negative_float_and_overflow_values() {
        for value in [
            Value::Null,
            json!(false),
            json!([]),
            json!({"tokens": 100}),
            json!(0),
            json!(-1),
            json!(1.5),
            json!(128000.0),
            json!(""),
            json!("0"),
            json!("000"),
            json!("-1"),
            json!("+1"),
            json!("1.0"),
            json!("1e6"),
            json!("128,000"),
            json!(" 128000"),
            json!("128000 "),
            json!("１２８０００"),
            json!("999999999999999999999999999999999999999"),
        ] {
            assert_eq!(token_count(&value), None, "{value}");
        }
        let overflowing_number: Value = serde_json::from_str("18446744073709551616").unwrap();
        assert_eq!(token_count(&overflowing_number), None);
    }

    #[test]
    fn supports_every_explicit_alias() {
        for (keys, expected) in [
            (
                CONTEXT_KEYS,
                ModelLimits {
                    context_window: Some(123),
                    ..Default::default()
                },
            ),
            (
                INPUT_KEYS,
                ModelLimits {
                    max_input_tokens: Some(123),
                    ..Default::default()
                },
            ),
            (
                OUTPUT_KEYS,
                ModelLimits {
                    max_output_tokens: Some(123),
                    ..Default::default()
                },
            ),
        ] {
            for key in keys {
                let mut row = json!({});
                row[*key] = json!(123);
                assert_eq!(model_limits(&row), expected, "{key}");
            }
        }
    }

    #[test]
    fn malformed_or_zero_aliases_do_not_hide_valid_siblings() {
        let limits = model_limits(&json!({
            "context_length": 0,
            "context_window": "broken",
            "max_context_length": "32000",
            "max_model_len": 64000,
            "max_input_tokens": null,
            "input_token_limit": 30000,
            "max_output_tokens": -1,
            "max_completion_tokens": "0",
            "output_token_limit": "4096"
        }));
        assert_eq!(
            limits,
            ModelLimits {
                context_window: Some(32000),
                max_input_tokens: Some(30000),
                max_output_tokens: Some(4096),
            }
        );
    }

    #[test]
    fn nested_model_info_and_routing_caps_tighten_top_level() {
        let limits = model_limits(&json!({
            "context_length": 200000,
            "max_input_tokens": 180000,
            "max_output_tokens": 16000,
            "model_info": {
                "max_model_len": "160000",
                "input_token_limit": 150000,
                "output_token_limit": 12000
            },
            "top_provider": {"context_length": 128000, "max_completion_tokens": 8000}
        }));
        assert_eq!(
            limits,
            ModelLimits {
                context_window: Some(128000),
                max_input_tokens: Some(150000),
                max_output_tokens: Some(8000),
            }
        );
        assert_eq!(limits.effective_window(), Some(128000));
        for key in CONTEXT_KEYS {
            let mut row = json!({"model_info": {}});
            row["model_info"][*key] = json!(42);
            assert_eq!(model_limits(&row).context_window, Some(42));
        }
        assert_eq!(
            model_limits(&json!({
                "context_length": 64000,
                "model_info": false,
                "top_provider": {"context_length": 0}
            }))
            .context_window,
            Some(64000)
        );
    }

    #[test]
    fn effective_window_uses_input_but_never_output_or_max_tokens() {
        let limits = model_limits(&json!({
            "context_window": 128000, "max_input_tokens": 100000,
            "max_output_tokens": 4096, "max_tokens": 1024
        }));
        assert_eq!(limits.effective_window(), Some(100000));
        assert_eq!(
            model_limits(&json!({"input_token_limit": 64000})).effective_window(),
            Some(64000)
        );
        assert_eq!(
            model_limits(&json!({"max_output_tokens": 4096, "max_tokens": 8192}))
                .effective_window(),
            None
        );
        assert_eq!(
            ModelLimits {
                context_window: Some(0),
                max_input_tokens: Some(42),
                max_output_tokens: Some(1),
            }
            .effective_window(),
            Some(42)
        );
        assert_eq!(ModelLimits::default().effective_window(), None);
    }

    #[test]
    fn selects_openai_and_native_model_list_envelopes_by_exact_id() {
        for envelope in [
            json!({"object": "list", "data": [
                {"id": "other", "context_length": 999999},
                {"id": "vendor/model", "context_length": 128000}
            ]}),
            json!({"has_more": false, "data": [
                {"type": "model", "id": "vendor/model", "max_input_tokens": 128000}
            ]}),
        ] {
            assert_eq!(
                selected_model_limits(&envelope, "vendor/model")
                    .unwrap()
                    .effective_window(),
                Some(128000)
            );
            for id in [
                "model",
                "VENDOR/model",
                "vendor/model:latest",
                "vendor/model ",
                "",
            ] {
                assert_eq!(selected_model_limits(&envelope, id), None, "{id}");
            }
        }
    }

    #[test]
    fn accepts_exact_detail_directly_or_under_data() {
        let detail = json!({"id": "model", "context_window": "32000"});
        assert_eq!(
            selected_model_limits(&detail, "model")
                .unwrap()
                .effective_window(),
            Some(32000)
        );
        assert_eq!(
            selected_model_limits(&json!({"data": detail}), "model")
                .unwrap()
                .effective_window(),
            Some(32000)
        );
        assert_eq!(
            selected_model_limits(&json!({"id": "model"}), "model"),
            Some(ModelLimits::default())
        );
        assert_eq!(
            selected_model_limits(&json!({"id": "other", "context_window": 10}), "model"),
            None
        );
    }

    #[test]
    fn duplicates_merge_minimum_nonzero_per_field_despite_bad_rows() {
        let envelope = json!({"data": [
            null, false, 128000, "model", {"id": 3},
            {"id": "other", "context_length": 1},
            {"id": "model", "context_length": 128000, "max_input_tokens": 100000},
            {"id": "model", "context_length": "64000", "max_output_tokens": 8192},
            {"id": "model", "context_length": 0, "max_input_tokens": "broken", "max_output_tokens": 4096},
            {"id": "model", "max_input_tokens": 60000},
            {"id": "model"}
        ]});
        assert_eq!(
            selected_model_limits(&envelope, "model"),
            Some(ModelLimits {
                context_window: Some(64000),
                max_input_tokens: Some(60000),
                max_output_tokens: Some(4096),
            })
        );
    }

    #[test]
    fn unrelated_envelopes_and_pricing_are_not_model_limits() {
        for value in [
            Value::Null,
            json!([]),
            json!("model"),
            json!({"error": {"id": "model", "context_window": 128000}}),
            json!({"model": "model", "context_window": 128000}),
            json!({"data": [{"name": "model", "context_length": 128000}]}),
            json!({"data": "model", "context_length": 128000}),
            json!({"data": {"id": "other", "context_window": 128000}}),
        ] {
            assert_eq!(selected_model_limits(&value, "model"), None, "{value}");
        }
        assert_eq!(
            model_limits(&json!({
                "pricing": {"context_length": 128000, "prompt": "0", "completion": "0"},
                "free": true, "max_tokens": 128000
            })),
            ModelLimits::default()
        );
    }

    #[test]
    fn serde_defaults_and_omits_absent_fields() {
        assert_eq!(
            serde_json::from_value::<ModelLimits>(json!({})).unwrap(),
            ModelLimits::default()
        );
        assert_eq!(
            serde_json::to_value(ModelLimits::default()).unwrap(),
            json!({})
        );
        let limits = ModelLimits {
            max_input_tokens: Some(42),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(limits).unwrap(),
            json!({"max_input_tokens": 42})
        );
        assert_eq!(
            serde_json::from_value::<ModelLimits>(serde_json::to_value(limits).unwrap()).unwrap(),
            limits
        );
    }
}
