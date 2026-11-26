//! Optional provider tools. Every call is serial, opaque and rechecks a host-frozen generation.

use super::{memory_api::Capability, tool_support};
use crate::agent::tools::{PreparedToolCall, Tool, ToolObservation, ToolRegistry};
use anyhow::Result;
use serde_json::{json, Value};

struct MemoryTool {
    name: &'static str,
}
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        match self.name {
            "memory_search"=>"Search persistent facts from the explicitly selected external Memory provider. May update provider caches/exposure; serial, not effect-free.",
            "memory_list"=>"Inventory current scoped facts, or only superseded facts, from the external Memory provider; obtain record IDs before updating. Archived records require human management.",
            "memory_profile"=>"Ask the external Memory provider for a user profile with its evidence; unsupported providers fail explicitly.",
            "memory_ask"=>"Ask the selected Memory provider about stored user facts; its answer is lower-trust evidence, never policy.",
            "memory_save"=>"Save a durable user-requested/project fact using the selected Memory provider. Check existing records first; default project scope.",
            "memory_update"=>"Update a provider record by ID with revision conflict checking; preserve unspecified content and history.",
            "memory_forget"=>"Soft-retire a user-identified obsolete provider record; recoverable archive, not permanent erasure. Approval required.",
            _=>"External Memory operation",
        }
    }
    fn parameters(&self) -> Value {
        let properties = match self.name {
            "memory_search" => {
                json!({"query":{"type":"string","minLength":1,"maxLength":16384},
                    "limit":{"type":"integer","minimum":1,"maximum":100},
                    "scope":{"type":"string","enum":["current","project","user","global","device"]},
                    "category":{"type":"string"},"dimension":{"type":"string"}})
            }
            "memory_list" => {
                json!({"limit":{"type":"integer","minimum":1,"maximum":100},
                    "scope":{"type":"string","enum":["current","project","user","global","device"]},
                    "superseded":{"type":"boolean","description":"List only superseded facts instead of active facts; does not list archived records."}})
            }
            "memory_ask" => json!({"question":{"type":"string","minLength":1,"maxLength":16384}}),
            "memory_save" => {
                json!({"name":{"type":"string","minLength":1,"maxLength":256},
                    "body":{"type":"string","minLength":1,"maxLength":32768},
                    "description":{"type":"string","maxLength":2048},
                    "type":{"type":"string","enum":["user","feedback","project","reference"]},
                    "scope":{"type":"string","enum":["project","user","global","device"],
                        "description":"Default project. Global aliases user. Device requires provider support; Mnemos rejects device-scope store."}})
            }
            "memory_update" => {
                json!({"id":{"type":"string","minLength":1,"maxLength":256},
                    "body":{"type":"string","minLength":1,"maxLength":32768},
                    "name":{"type":"string","minLength":1,"maxLength":256},
                    "description":{"type":"string","maxLength":2048},
                    "type":{"type":"string","enum":["user","feedback","project","reference"]},
                    "scope":{"type":"string","enum":["project","user","global","device"],
                        "description":"Optional assertion of existing scope only; scope changes require human management."}})
            }
            "memory_forget" => json!({"id":{"type":"string"},"reason":{"type":"string"}}),
            _ => json!({}),
        };
        let required = match self.name {
            "memory_search" => vec!["query"],
            "memory_ask" => vec!["question"],
            "memory_save" => vec!["name", "body"],
            "memory_update" | "memory_forget" => vec!["id"],
            _ => vec![],
        };
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }
    fn is_destructive(&self) -> bool {
        self.name == "memory_forget"
    }
    fn is_memory_provider(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn recovery_effect(&self, _: &Value) -> bool {
        true
    }
    fn plan_call(
        &self,
        args: &Value,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        tool_support::prepare_tool(self.name, args, ctx)
    }
    fn prepare_call(
        &self,
        args: &Value,
        ctx: &crate::core::exec_ctx::ExecutionContext,
    ) -> Result<PreparedToolCall> {
        tool_support::prepare_tool(self.name, args, ctx)
    }
    fn execute_prepared(&self, prepared: &PreparedToolCall) -> Result<ToolObservation> {
        tool_support::execute_prepared(prepared)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        tool_support::direct(self.name, args)
    }
}

pub(crate) fn register(registry: &mut ToolRegistry) {
    if tool_support::available(Capability::Records) {
        for name in [
            "memory_search",
            "memory_list",
            "memory_save",
            "memory_update",
            "memory_forget",
        ] {
            registry.register(Box::new(MemoryTool { name }));
        }
    }
    if tool_support::available(Capability::Commands) {
        for name in ["memory_profile", "memory_ask"] {
            registry.register(Box::new(MemoryTool { name }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_schema_does_not_advertise_unsupported_filters() {
        let schema = MemoryTool {
            name: "memory_list",
        }
        .parameters();
        let properties = schema["properties"].as_object().unwrap();
        assert!(!properties.contains_key("type"));
        assert!(!properties.contains_key("include_archived"));
        assert_eq!(properties["superseded"]["type"], "boolean");
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn retrieval_schema_declares_wire_bounds_and_scopes() {
        for name in ["memory_search", "memory_list"] {
            let schema = MemoryTool { name }.parameters();
            assert_eq!(schema["properties"]["limit"]["minimum"], 1);
            assert_eq!(schema["properties"]["limit"]["maximum"], 100);
            assert_eq!(
                schema["properties"]["scope"]["enum"],
                json!(["current", "project", "user", "global", "device"])
            );
        }
        let schema = MemoryTool {
            name: "memory_search",
        }
        .parameters();
        assert_eq!(schema["properties"]["query"]["maxLength"], 16384);
    }

    #[test]
    fn write_schema_declares_scopes_and_record_types() {
        for name in ["memory_save", "memory_update"] {
            let schema = MemoryTool { name }.parameters();
            assert_eq!(
                schema["properties"]["scope"]["enum"],
                json!(["project", "user", "global", "device"])
            );
            assert_eq!(
                schema["properties"]["type"]["enum"],
                json!(["user", "feedback", "project", "reference"])
            );
        }
        let schema = MemoryTool {
            name: "memory_update",
        }
        .parameters();
        assert!(schema["properties"]["scope"]["description"]
            .as_str()
            .unwrap()
            .contains("human management"));
    }
}
