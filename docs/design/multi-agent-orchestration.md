# Multi-Agent Orchestration — Design Doc

Trạng thái: draft v1 · Tác giả: nghiên cứu tổng hợp (codebase aizen + SOTA research) · 2026-10-06

## 0. Phát hiện nền tảng (đọc trước khi tranh luận kiến trúc)

Research SOTA (Anthropic engineering, Cognition, LangGraph, AutoGen 0.4, Temporal, OTP)
đưa ra một kết luận **ngược trực giác** mà mọi quyết định dưới đây bám vào:

> **"Nhiều agent giao tiếp tự do hơn" KHÔNG phải là cách làm hệ multi-agent mạnh hơn
> cho coding.**
>
> - Anthropic (built-multi-agent-research-system): multi-agent thắng **90.2%** trên
>   research eval nhưng tốn **~15× tokens**; token usage giải thích **80% variance**
>   hiệu năng. Họ thắng vì bài toán research là *breadth-first, fan-out được, kết quả
>   nén được*. Họ tự nói: *"most coding tasks involve fewer truly parallelizable
>   tasks... LLM agents are not yet great at coordinating in real time"*.
> - Cognition (Don't Build Multi-Agents): coding là chuỗi quyết định phụ thuộc lẫn
>   nhau; subagent song song thiếu shared context sinh ra code rời rạc (ví dụ Flappy
>   Bird: subagent A vẽ background kiểu Mario, subagent B vẽ nhân vật kiểu khác).
>
> **Hệ quả cho aizen:** mạnh hơn = (a) **durability** để không mất việc đã làm,
> (b) **fan-out read-only thông minh** (điểm multi-agent thắng thật),
> (c) **shared context nén** thay vì message passing tự do,
> (d) **giám sát + eval có đo lường** thay vì thêm agent.

Nguồn: https://www.anthropic.com/engineering/built-multi-agent-research-system ·
https://cognition.ai/blog/dont-build-multi-agents ·
https://docs.langchain.com/oss/python/langgraph/graph-api ·
https://microsoft.github.io/autogen/stable/user-guide/ · https://docs.temporal.io/workflows

## 1. Đánh giá hiện trạng aizen (đã đọc code)

Đã có sẵn, không cần xây lại:

| Thứ | Ở đâu | Ghi chú |
|---|---|---|
| Role-scoped tool registry | `roles.rs` (7 roles) | brief + grant cùng một bảng, không lệch |
| Wave scheduling trên DAG `after` | `workflow.rs` `waves()` (Kahn layering) | validate cycle/self-ref tốt |
| Singular writer per wave | `enforce_singular_writer` | an toàn có chủ đích |
| Blackboard sibling (file append-only) | `blackboard.rs` | wave sau đọc full report wave trước |
| Context pack lúc spawn | `context_pack.rs` | reads + findings + todo → brief |
| Verify gate cho writer + 1 retry + restore | `task_tool.rs` | W14 |
| Derived TurnCancel per child | `task_tool.rs` | `/workflows stop #id` |
| Slot cap toàn máy | `SubagentSlot` | trả lỗi mềm khi đầy |
| Nudge mid-turn (infra) | `NudgeRole`, `push_nudge` | chưa dùng cho child |

Lỗ hổng thật (theo thứ tự đau):

1. **Không có durability** — mọi run sống trong RAM; crash = mất toàn bộ sub-work đã
   trả token. (Temporal gọi đây là thứ phải có ĐẦU TIÊN.)
2. **Wave barrier** — task trong wave k+1 chờ cả wave k xong kể cả khi dependency của
   nó đã xong (`schedule()` chạy `join_all` theo chunk, rồi writer, rồi mới sang wave).
3. **Blackboard chỉ đọc được khi spawn** — `env_line()` chụp list 1 lần; child đang
   chạy không thấy finding mới của sibling.
4. **Không có eval** — không đo được "nâng cấp X có tốt hơn không".
5. **Effort không scale** — prompt không có quy tắc "task nhỏ = ít agent" (Anthropic
   gặp failure mode spawn 50 agent cho query đơn giản).

## 2. Nguyên tắc thiết kế (từ research, không phải ý kiến)

- **P1 — Write path single-agent.** Implement/refactor giữ trong 1 daedalus (hoặc
  1 chain có handoff context đầy đủ). Không bao giờ 2 writer tự do. (Cognition P1)
- **P2 — Multi-agent chỉ ở chỗ nén được.** Fan-out read-only (explore, review,
  verify); mỗi child trả về **findings đã nén**, không trả raw transcript.
  (Anthropic "intelligent filter")
- **P3 — Share context, không share message.** Sibling nhìn thấy cùng một blackboard
  + cùng run log thay vì nhắn tin 1-1 tự do. (Cognition P1, LangGraph channels)
- **P4 — Mọi side-effect ghi log một lần, replay được.** Orchestration là hàm thuần
  của event log. (Temporal Durable Execution)
- **P5 — Có budget + supervision.** Mỗi child có step/token/time budget; restart có
  max-intensity chống vòng lặp đốt token. (OTP + Anthropic lesson)
- **P6 — Đo trước khi tối ưu.** Eval harness là deliverable của phase 1, không phải
  việc "sau này".

## 3. Kiến trúc mục tiêu

```text
                 ┌───────────────────────────────────────────────┐
                 │ Coordinator (planner = pure fn của EventLog)  │
                 │  ready-queue · dependency resolver · budgeter │
                 └──────┬───────────────────────┬────────────────┘
                        │ spawn (budget+scope)  │ nudge (signal file)
              ┌─────────▼───────┐        ┌──────▼──────┐
              │ fan-out read-only│       │ write chain  │  ← P1: single writer
              │ argus · nemesis  │       │ daedalus     │
              │ clio · mnemosyne │       │ (→ themis)   │
              └─────────┬───────┘        └──────┬──────┘
                        │ append                │ append
                 ┌──────▼───────────────────────▼──────┐
                 │  EventLog  .aizen/runs/<id>.jsonl    │  ← P4, mọi thứ replay được
                 │  Blackboard  scratch/blackboard/<id> │  ← P3, shared context nén
                 └─────────────────────────────────────┘
                        ▲  resume = replay log, skip activity đã có kết quả
```

Khác biệt then chốt so với kiến trúc "mesh" tự do: **không có pub/sub giữa agent**,
không có child-spawn-child. Communication = blackboard có thứ tự + nudge từ
coordinator. Đây là sự kết hợp LangGraph (channels + reducer) và Temporal
(event sourcing), né cái mà Anthropic/Cognition đã chỉ ra là thua.

## 4. Roadmap 4 phase (mỗi phase shippable, đo được)

### Phase 1 — Durable run + eval harness *(nền tảng, không có nó mọi thứ khác vô nghĩa)*
- **Event log**: `src/agent/runlog.rs` mới — append-only JSONL
  `.aizen/runs/<run-id>.jsonl`; event = `{kind: llm|tool|spawn|note|signal, …, hash}`.
  Ghi sau mỗi super-step hoàn chỉnh.
- **Resume**: `aizen resume <run-id>` — replay log, skip event đã có kết quả, chỉ gọi
  lại LLM/tool cho phần chưa chạy. Orchestrator tách thành pure fn `plan(&[Event]) ->
  Vec<Action>`.
- **Status query**: `aizen status <run-id>` đọc log, không ghi (Temporal Query).
- **Eval harness**: `tests/eval/` — ~20 task coding thật (small/medium/large),
  LLM-judge 1 prompt, rubric: correctness / completeness / tool-efficiency / wall-clock.
  Đây là thước đo cho mọi phase sau.
- Độ chạm: `runlog.rs` (mới), `mod.rs` (hook ghi), `cli/` (2 subcommand), `tests/eval/`.
- Done-khi: kill -9 giữa một workflow 5 task, `resume` chạy tiếp đúng chỗ chết, không
  gọi lại LLM cho event đã ghi. Eval có baseline con số.

### Phase 2 — Live blackboard + dependency-driven scheduling *(cảm giác "phối hợp nhịp nhàng" nằm ở đây)*
- **Blackboard v2**: giữ file append-only nhưng refresh `env_line` mỗi lần child mở
  đầu một tool-turn (thay vì chụp 1 lần lúc spawn). Child đọc finding của sibling
  **giữa run**.
- **Nudge từ coordinator**: dùng sẵn `push_nudge` + một `signals.jsonl` per-run;
  parent (hoặc user qua `/workflows nudge #id "..."`) inject instruction vào child đang
  chạy mà không kill nó.
- **Bỏ wave barrier**: `schedule()` chuyển từ Kahn-layer-join_all sang **ready-queue**:
  task vào hàng đợi ngay khi mọi `after` của nó hoàn thành, slot rảnh là chạy. Vẫn giữ
  `enforce_singular_writer` (P1).
- **Slot queue**: `SubagentSlot::Full` → park task vào queue thay vì trả lỗi mềm.
- Độ chạm: `blackboard.rs`, `workflow.rs` (schedule), `task_tool.rs` (slot, nudge hook).
- Done-khi: workflow A→B, A→C (B,C độc lập) có wall-clock ≈ max(B,C) chứ không phải
  tổng wave; một child giữa run đọc được finding vừa ghi của sibling; eval cho thấy
  wall-clock giảm mà quality không tụt.

### Phase 3 — Supervision + effort scaling *(độ tin cậy)*
- **Restart policy per task**: `one_for_one` mặc định; `max_intensity` (vd 2 lần/5 phút)
  chống retry-loop đốt token. FAIL lần 2 → **escalate**: metis re-plan với full failure
  context → daedalus chạy theo plan mới (thay vì restore checkpoint rồi dừng).
- **Effort-scaling trong prompt** (Anthropic lesson): bảng trong system prompt của
  coordinator — fact-finding = 1 agent/3–10 tool call; so sánh = 2–4 subagent; research
  sâu > 10. Refuse/log khi spec spawn vượt budget không lý do.
- **Diff-feedback retry**: retry của writer kèm nemesis diff cụ thể, không chỉ partial
  report.
- **Per-model concurrency budget**: 2 child cùng gọi 1 gateway nhỏ vẫn xếp hàng.
- Độ chạm: `workflow.rs` (fix_loop → supervise), `roles.rs` (effort table),
  `task_tool.rs` (retry brief), `cli_config.rs` (per-model budget).
- Done-khi: một task cố tình fail 2 lần được re-plan và hoàn thành; eval quality tăng,
  token/run không tăng quá ngưỡng đã định.

### Phase 4 — Metrics tự cải thiện *(dài hạn)*
- Ghi per-role stats (wall-clock, tokens, pass-rate) vào `.aizen` mỗi run; scheduler
  ưu tiên pairing role×model đã chứng minh hiệu quả trên chính repo này.
- **Không** làm agent-ID/pub-sub multi-tenant (AutoGen) cho tới khi thật sự cần nhiều
  phiên song song — research cho thấy đó là phức tạp không cần thiết lúc này.

## 5. Những thứ CỤ THỂ không làm (và lý do)

- **Không pub/sub tự do giữa agent** (AutoGen topic): Cognition + Anthropic đều chỉ ra
  đó là nguồn context-fragmentation trong coding. Shared blackboard là đủ.
- **Không bỏ depth-cap-1** trừ khi có use-case chứng minh; nested spawn làm run-log
  khó replay (P4) và budget khó kiểm soát (P5).
- **Không parallel writers** trên cùng tree — ngay cả khi file-set rời nhau, merge
  conflict + verify-gate chồng nhau đắt hơn lợi ích. Chia repo thành worktree nếu thật
  sự cần (việc của user, không phải orchestrator).
- **Không nhét Temporal server / external runtime** — mọi primitive phải chạy trong
  một binary, event log là file JSONL local.

## 6. Ma trận quyết định nhanh

| Tình huống | Quyết định | Căn cứ |
|---|---|---|
| Task read-heavy, fan-out được | Multi-agent (workflow fanout) | Anthropic 90.2% |
| Task write-heavy, quyết định dây chuyền | Single daedalus chain | Cognition P1 |
| Cần song song hoá write | Chia worktree thủ công, vẫn 1 writer/tree | Cognition P1 |
| Child fail | Retry có giới hạn → escalate re-plan | OTP + Anthropic resume |
| Không chắc nên spawn bao nhiêu agent | Effort table trong prompt | Anthropic lesson |
| Muốn biết nâng cấp có tốt không | Chạy eval, so baseline | P6 |

## 7. Câu hỏi mở cần dawn quyết

1. Eval harness: chọn ~20 task từ repo nào? (aizen tự test trên chính nó, hay tập
   repo mẫu cố định?) — quyết định tính ổn định của baseline.
2. Budget mặc định cho effort table (token/run, wall-clock/run) — cần con số khởi đầu
   để supervise có ngưỡng.
3. Phase 1 làm trên branch riêng `feat/durable-runs` hay thẳng `main`?
