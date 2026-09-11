# Chat agent context, routing, and clarification

## Implementation

### Thread continuity (`sde` + IDE)

- `run_headless_detailed` now returns the thread id produced by the react headless runner, not only a pre-requested id.
- `skippr chat send` passes the rendered user message as `headless_prompt`, enables `stream_jsonl` for `--output jsonl`, and sets `SKIPPR_HEADLESS_QUESTION` for the data-engineer suite entrypoints.
- Workspace react dependencies use published `git/path react` 1.4.0; use `cargo` for sibling `../react` during local development.

### Ask routing (`skipprd-react-suite-data-engineer`)

- Ask system prompt adds explicit **simple vs complex** routing: bounded catalog + query path for simple questions; `ask_user` for ambiguous complex questions before broad exploration.
- `headless_mode_enabled()` is false when `SKIPPR_EXECUTION_SURFACE=ide_chat`, enabling `ask_user` / `ask_approval` in IDE chat.

### IDE agent host (`skippr-ide`)

- `SkipprCliAgent` captures `thread_id` from `thread_assigned` and `ChatSummary` JSONL, persists under `.skippr/ide/chat-thread.json`, and passes `--thread` on follow-up turns.
- `await_user` / `await_approval` JSONL maps to `SessionInputRequested`; user replies resume via `chat send --thread`.
- SQL panel `askDataQuestion` persists thread id in extension `globalState` and passes `--thread` on subsequent asks.

### React transport

- Headless resume with a real user message uses the `user` frame (not `open`) when continuing a thread.

## Human review (IDE)

Prerequisites: local `skipprd` build, Extension Dev Host, workspace `/Users/huders2000/Desktop/sdgsdgsdsd`, pipeline `bike_hire`, Skippr Agent mode **ask**.

| Step | Action | Pass |
|------|--------|------|
| T1 | Ask “How many bikes are in the hire dataset?” then “What table did you use?” | Second answer references first turn; CLI log shows `chat resuming thread <uuid>` |
| T2 | After turn 1, confirm `ChatSummary` / logs contain `thread_id` | Non-empty uuid |
| T3 | Simple count question on main fact table | SQL + answer; short tool path (config/metadata → query) |
| T4 | “Why did revenue drop?” without dates | Agent asks clarifying questions before guessing |
| T5 | Answer a mid-run clarification prompt | Run continues on same thread with updated answer |
| T6 | Close chat, open new Skippr Agent session, ask follow-up | Same thread id resumed from `.skippr/ide/chat-thread.json` |
| T7 | SQL panel “Ask data question” twice | Second invocation passes `--thread` |

For S3-backed thread storage debugging, use `AWS_PROFILE=skippr-prod` when inspecting tenant thread objects in the configured bucket.
