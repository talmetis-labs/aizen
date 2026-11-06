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
            "memory_list"=>"Inventory scoped facts from the external Memory provider; obtain record IDs before updating. May have provider-side effects.",
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
                json!({"query":{"type":"string"},"limit":{"type":"integer"},"scope":{"type":"string"},"category":{"type":"string"}})
            }
            "memory_list" => {
                json!({"limit":{"type":"integer"},"scope":{"type":"string"},"type":{"type":"string"},"include_archived":{"type":"boolean"}})
            }
            "memory_ask" => json!({"question":{"type":"string"}}),
            "memory_save" => {
                json!({"name":{"type":"string"},"body":{"type":"string"},"description":{"type":"string"},"type":{"type":"string"},"scope":{"type":"string"}})
            }
            "memory_update" => {
                json!({"id":{"type":"string"},"body":{"type":"string"},"name":{"type":"string"},"description":{"type":"string"},"type":{"type":"string"},"scope":{"type":"string"}})
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
