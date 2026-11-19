# Long-term memory with HeliconSearch (Rust SDK)

The Rust SDK can give your agents long-term memory served by
[HeliconSearch](https://github.com/intel-innersource/applications.ai.ainas.heliconsearch). It is off until you add one
configuration file. Your application code does not change: the same `Harness::builder()`, `session`,
`run`, `send` and `close` calls apply.

It is built on a small public seam, the [`Extension`](#the-extension-seam) trait, so the same hooks can serve other
components. This extension is memory only; media search is not part of the Rust SDK.

With it on:

- **Recall.** Before each message is sent, memories relevant to it are retrieved and added to that turn's
  `instruction_addendum`. The addendum is not part of the saved conversation, so recalled text is never stored again.
- **A new session's first turn** also gets a short cross-session summary (recent sessions, pinned facts,
  project workflows). It is shown once, on that first turn, and not repeated.
- **Capture.** After each completed turn, the new conversation is archived verbatim on HeliconSearch and facts
  are extracted from it. This follows the engine's journal, so it works the same for `run`, `send`, streams, and for
  turns nobody awaited.
- **Memory tools.** Agents get `helicon__memory_recall`, `helicon__memory_get`, `helicon__memory_forget` and
  `helicon__skill_get` (and `helicon__memory_store` if you enable it).
- **The engine's built-in memory is turned off** for agents that get these tools (`memory(false)`), so an agent never
  has two memories. Set `replaceBuiltinMemory` to `false` to keep both.
- **Session summary.** A session is ended on HeliconSearch, which then writes its summary, when you call
  `session.close()`, when you shut the harness down, when it has been idle for 30 minutes, and, for a run that
  died, at the next start.

## Turn it on

Create `helicon-memory.json` in the harness state directory (`HarnessBuilder::state_dir`; or point
`OPENWORKER_HELICON_CONFIG` at a file elsewhere; a packaged application can also ship it next to its executable, in
`resources/` or `../Resources/`):

```json
{
  "baseUrl": "http://127.0.0.1:5001",
  "apiKey": "${HELICON_TENANT_TOKEN}",
  "agentId": "my-app",
  "captureMode": "operator"
}
```

`${NAME}` is read from the process environment when the harness starts, so the token need not be in the file.
Without a token the memory stays off and a warning is printed to stderr; no file, or `"enabled": false`, means memory
is off and the SDK is untouched. **This is the same file the TypeScript SDK reads**: one file serves an application
written in either language (options that configure media search are accepted and ignored).

All memories belong to the tenant the token identifies. `agentId` and `projectId` are partitions inside that
tenant: everyone who shares a token shares its memories.

```rust
let harness = Harness::builder()
    .state_dir("state")            // holds helicon-memory.json
    .executable(host_binary)
    .embedded()
    .await?;

// An agent registered after start gets the memory tools and their usage note.
let agent = harness
    .create_agent(Agent::new("assistant").instructions("Be helpful."))
    .await?;
let session = agent.new_session().await?;
session.run("Remember that I prefer dark mode.").await?;
session.close().await?;      // sends what is pending and ends the session on HeliconSearch
harness.shutdown().await?;
```

### Options

| Option | Default | Meaning |
| --- | --- | --- |
| `baseUrl` | required | HeliconSearch address. |
| `apiKey` | required | Tenant token: the token itself, or `"${HELICON_TENANT_TOKEN}"`. |
| `agentId` / `projectId` | `openworker` / `""` | Where memories are written and read. |
| `autoRecall` | `true` | Recall into every turn. |
| `autoCapture` | `true` | Archive and extract after every turn. |
| `captureMode` | `operator` | `operator`: HeliconSearch's model extracts, merges and updates facts from the whole conversation. `direct`: no model; user sentences that match a few keyword rules are stored as they are (at most 3 a turn), and HeliconSearch still extracts the archived conversation when the session ends, so facts can be stored twice. |
| `sessionBridge` | `true` | Add the cross-session summary to a new session's first turn. |
| `hookTimeoutMs` | `15000` | The longest a turn waits for memory before it goes on without it. |
| `endOnShutdown` | `true` | `Harness::shutdown` ends the sessions this run worked on and did not close. |
| `idleEndMs` | `1800000` | A session with no turn for this long (ms) is ended; a session an earlier run left open is ended at the next start. `0`: never. |
| `agents` | all | Ids of the agents (`Agent::new("<id>")`) that get memory. Every other agent of the harness is left alone: its sessions are neither recalled into nor archived. |
| `closeWaitMs` | `0` | How long `close()` waits for this process's own fact extraction (`0`: not at all). |
| `replaceBuiltinMemory` | `true` | Agents get the HeliconSearch tools instead of the engine's built-in `memory_*` tools. |
| `memoryStoreTool` | `false` | Also give agents a `memory_store` tool. |
| `recallTopK`, `recallMinScore`, `recallMaxChars`, `recallGate`, `storeGate` | see the TypeScript guide | Recall and extraction tuning. |
| `debug` | `false` | Log every request to `<state_dir>/helicon-memory/adapter.log` (warnings are always logged). |

## What to know

- **Memory never blocks or fails a turn.** If HeliconSearch is slow, down or returns an error, the turn is sent
  without recalled memory. Whatever could not be archived is sent the next time that session has activity, or when
  the session is resumed in a later run.
- **Tool calls are approvals.** The memory tools are ordinary application tools, so the agent's approval mode applies:
  with the default `Approvals::Always` the user is asked before the agent searches or forgets. Recall and capture are
  not tool calls and need no approval.
- **Agents.** When memory is on, `Harness::create_agent` adds the tool names and a short usage note to the agent's
  `tools` and `instructions`. Agent definitions are immutable per id, so turning memory on or off for an existing
  agent needs a new agent id.
- **Existing sessions are back-filled.** A session that predates the configuration is archived from its first message
  the next time it has a turn.
- **Ending is not final.** A session that was ended and then gets another turn is archived into the same
  HeliconSearch session and ended again when it goes quiet again.
- **`close()` does not wait for HeliconSearch to finish.** It sends what is pending (the verbatim archive) and ends the
  session; HeliconSearch extracts facts and writes the summary on its own. Set `closeWaitMs` only for a short-lived
  program that wants its own extraction finished before it exits.
- **Recalled memory is data, not instructions.** Both the recall and the first-turn summary are added with a notice
  saying so, and escaped.
- **Everything said is sent to HeliconSearch** and stored there verbatim, with the facts extracted from it. Tool calls
  and their results are archived too, except the memory tools' own.
- **A retry gets the same text.** The recall for one submission is remembered, so resending a submission id (after
  `Error::OutcomeUnknown`) sends the same turn context. The memory is kept in process: after an application restart a
  resend recalls again, and if the answer differs the engine refuses the id as a changed turn context — resend with a
  new submission id.
- **Guardrails see the recalled text.** The engine runs its safety hooks over the whole turn addendum, so a guardrail
  that blocks on a remembered sentence blocks that turn, every time that sentence is recalled.
- **`autoCapture: false` sends nothing**, not even when a session is closed or the harness shut down.
- **A session whose history could not be archived (HeliconSearch down) is not ended on HeliconSearch.** The next idle
  sweep, close or shutdown tries again.

## Limits

- **Embedded and managed hosts only.** A `Harness::connect` service client has no extension hooks.
- **Agents given to `HarnessBuilder::agent` are compiled by the host before application tools can be registered**, so
  they do not get the memory tools or the usage note; their built-in memory is still turned off and their sessions are
  still recalled into and archived. Use `harness.create_agent` for agents that should have the tools. (If the
  extension cannot start, the reason is printed to stderr and the built-in memory of those agents stays off.)
- Recall covers the turns you send through a `Session`. Sub-agents and team members started by the engine, and turns
  the engine starts by itself (timers), are not given recalled memory and are not captured.
- Tool results are text. Images and documents a user attaches are archived as placeholders, not their bytes.
- An application that is killed (no `close()`, no `shutdown()`) has its open sessions ended at the next start, not at
  once.

## The extension seam

`openworker_sdk::extension::Extension` is a public trait with default no-op hooks. `HarnessBuilder::extension` attaches
your own. With no extension attached every hook is skipped and the SDK behaves exactly as before. Hooks are advisory: the SDK
never turns an extension's problem into an application error — a hook that panics is ignored and reported on stderr, and
a failing `on_start` drops that extension and the harness starts without it. A slow hook delays the call it hooks into,
so a hook bounds its own time.

| Hook | Called |
| --- | --- |
| `on_start(StartContext)` | when the harness is up: register tools (`Harness::register_tools`), subscribe to the journal |
| `decorate_agent(&mut Agent, &DecorateContext)` | before an agent is registered or compiled: add tools, text, turn the built-in memory off |
| `on_session_created`, `on_session_attached` | when a `Session` is created, or created or reattached (its journal is now delivered) |
| `before_send(TurnContext) -> Option<String>` | before each message is sent: return the `instruction_addendum` to use (the caller's own is in `turn.addendum`; keep it first). `submission_id` is stable for a retry |
| `before_close` | when `Session::close` is called |
| `on_shutdown` | when `Harness::shutdown` is called |

`before_send` only runs when the host reports `turn_context` (every current host does). The `run`, `run_streamed`,
`send` and `send_input` paths all call it.

## Troubleshooting

Look at `<state_dir>/helicon-memory/adapter.log`, and set `"debug": true` to see each request and the injected recall
block. Per-session progress is in `<state_dir>/helicon-memory/sessions/<session id>.json`; deleting a file makes the next
turn of that session send its whole history again (HeliconSearch ignores what it already holds).

## Tests

```sh
cargo test -p openworker-sdk --lib helicon                           # unit tests, fake HeliconSearch on loopback
cargo test -p openworker-sdk --features test-support --test helicon  # real engine, scripted model, fake HeliconSearch
# against a real HeliconSearch (uses an agent id of its own and removes what it stores):
HELICON_LIVE_URL=http://127.0.0.1:5001 HELICON_LIVE_TOKEN=hs_... \
  cargo test -p openworker-sdk --lib live -- --ignored --nocapture
```
