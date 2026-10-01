# Oracle Route — đã có sẵn, chỉ cần config

Aizen **đã có** Oracle (second-opinion model) và role-based model routing. Không cần code mới — chỉ cần set config.

## Cấu hình trong `~/.aizen/cli-config.json`

```json
{
  "roles": {
    "oracle": "kimi-k2-0711",           // model mạnh hơn cho self-review
    "subagent_default": "gpt-5.6-sol",  // model cho mọi sub-agent khi không override
    "summarizer": "gpt-5.6-sol",        // model cho compaction summary
    "apply": "kimi-k2-0711",            // model cho diff apply
    "pantheon": {                        // pin model theo role cụ thể
      "nemesis": "kimi-k2-0711",        // reviewer → model mạnh
      "themis": "gpt-5.6-sol",          // tester → model nhanh
      "argus": "gpt-5.6-sol"            // searcher → model nhanh
    }
  }
}
```

Hoặc env var (override config): `AIZEN_ORACLE_MODEL`, `AIZEN_SUBAGENT_DEFAULT_MODEL`, `AIZEN_PANTHEON_NEMESIS_MODEL`, v.v.

## Nguồn

- `src/core/cli_config.rs` — `RolesConfig` struct, `pantheon_endpoint()`
- `src/agent/mod.rs:652` — `roles.oracle` dùng trong self-review
- `src/agent/task_tool.rs:578` — `pantheon_endpoint` khi dispatch sub-agent

## Ví dụ dùng trong dispatch

```
task(role="nemesis", model="kimi-k2-0711", ...)  // override trực tiếp
workflow(fanout, tasks=[{role:"argus", ...}, {role:"nemesis", model:"kimi-k2-0711", ...}])
```
