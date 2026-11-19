# Long-term memory with HeliconSearch (Rust SDK)

The Rust SDK can give your agents long-term memory served by HeliconSearch, a memory and retrieval server you run
yourself. It is off until you add one configuration file. Your application code does not change: the same
`Harness::builder()`, `session`, `run`, `send` and `close` calls apply, except that agents are registered with
`harness.create_agent` (see [Limits](#limits)).

It is built on a small public seam, the [`Extension`](#the-extension-seam) trait, so the same hooks can serve other
components. This extension is memory only; media search is not part of the Rust SDK.

With it on:

- **Recall.** Before each message is sent, memories relevant to it are retrieved and added to that turn's
  `instruction_addendum`. The addendum is not part of the saved conversation, so recalled text is never stored again.
- **A new session's first turn** also gets a short cross-session summary (recent sessions, pinned facts, project
  workflows). It is shown once, on that first turn, and not repeated.
- **Capture.** After each completed turn, the new conversation is archived on HeliconSearch (as it was said, each turn
  up to `transcriptMaxCharsPerTurn` characters) and facts are extracted from it. This follows the engine's journal, so it works the same for `run`, `send`, streams, and for turns
  nobody awaited.
- **Memory tools.** Agents get `helicon__memory_recall`, `helicon__memory_get`, `helicon__memory_forget` and
  `helicon__skill_get` (and `helicon__memory_store` if you enable it).
- **The engine's built-in memory is turned off** for agents that get these tools (`memory(false)`), so an agent never
  has two memories. Set `replaceBuiltinMemory` to `false` to keep both.
- **Session summary.** A session is ended on HeliconSearch, which then writes its summary, when you call
  `session.close()`, when you shut the harness down, when it has been idle for 30 minutes, and, for a run that died,
  at the next start.

## Quick start

You need a HeliconSearch server you can reach (by default `http://127.0.0.1:5001`; ask whoever runs yours, or see its
own documentation), a model endpoint (any OpenAI-compatible chat-completions service), and a Rust build environment:
the toolchain the checkout pins (`rustup` installs it on the first `cargo` command) and a C toolchain (on Windows:
Visual Studio build tools and LLVM/Clang). The first build takes several minutes; after that the example runs in about
a minute. Run the commands from the `harness/` directory of the OpenWorker checkout (the one with `Cargo.toml`); the
`export` lines are for a POSIX shell (PowerShell: `$env:NAME = "value"`).

**1. Get a tenant token.** HeliconSearch refuses every memory call without one, and everything under one token is
one pool of memories. The administrator of the HeliconSearch server creates it once: HeliconSearch's own
`scripts/register_tenant.sh my-app` prints the token, or use its API (needs `curl` and `jq`):

```sh
HS=http://hs-host:5001
SESSION=$(curl -s -X POST $HS/v1/tenant/admin/login -H 'content-type: application/json' \
  -d '{"username":"admin","password":"<administrator password>"}' | jq -r .session)
curl -s -X POST $HS/v1/tenant/register -H "X-Admin-Session: $SESSION" -H 'content-type: application/json' \
  -d '{"username":"my-app","level":0}'        # the answer holds "token": "hs_..."
```

Change the factory administrator password of any server that holds real data. An application you ship to other people
carries one such token in its configuration, so that its users create nothing.

**2. Build the helper.** An agent with tools needs the `openworker-harness` helper next to your program, and the
memory tools are tools:

```sh
cargo build -p openworker-sdk --bin openworker-harness
```

**3. Run the example.** It tells the agent a fact in one session and asks about it in the next:

```sh
export HELICON_TENANT_TOKEN=hs_...        # the token from step 1
export OPENAI_API_KEY=...                 # your model endpoint's key
export OPENWORKER_MODEL=gpt-4o-mini       # an exact model ID of that endpoint
# optional: HELICON_URL=http://hs-host:5001   OPENWORKER_MODEL_URL=https://your-endpoint/v1
cargo run -p openworker-sdk --example helicon_memory
```

The second answer should name the tea from the first session. The example is
[helicon_memory.rs](../../../harness/harness-sdk/examples/helicon_memory.rs). It keeps its state in
`./.helicon-example-state/` and writes the configuration file the extension needs there, once: to change `HELICON_URL`
or any other setting afterwards, edit `./.helicon-example-state/helicon-memory.json` (or delete the directory to start
over). Its log is `./.helicon-example-state/helicon-memory/adapter.log`.

**4. Use it in your own application.** Put `helicon-memory.json` in your state directory (see
[Turn it on](#turn-it-on)) and register your agents with `harness.create_agent(..)`; nothing else changes.

**5. Check that it is working.** Set `"debug": true` and read `<state_dir>/helicon-memory/adapter.log` (the example
already does): an `adapter on:` line at start, then for each turn `auto_recall injected ...` (what was recalled; `no memories to
inject` when nothing relevant was found, as on a new tenant's first turns) and `archived turns=...` (what was sent), and
when a session closes `session end status=...` (the status HeliconSearch answered with). If memory is off for a
reason (a bad file, no token), the harness still starts and says why on stderr: `openworker: helicon-memory is off: ...`.
HeliconSearch extracts facts with a model, which takes seconds to minutes: a fact told in one session can be recalled
from other sessions once that has happened.

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

`${NAME}` is read from the process environment when the harness starts, so the token need not be in the file. Without
a token the memory stays off and a warning is printed to stderr; no file, or `"enabled": false`, means memory is off
and the SDK is untouched. The TypeScript SDK's HeliconSearch adapter reads the same file format, so one file can serve an
application written in either language (options that configure media search are accepted and ignored).

The token travels in an `Authorization` header: use `https` (or a network you trust) for a server that is not on this
machine.

All memories belong to the tenant the token identifies. `agentId` and `projectId` are partitions inside that tenant:
everyone who shares a token shares its memories.

```rust
let harness = Harness::builder()
    .state_dir(std::env::current_dir()?.join("state"))   // an absolute path; holds helicon-memory.json
    .executable(helper_binary)     // the openworker-harness helper (agents with tools need it)
    .embedded()
    .await?;

// An agent registered after start gets the memory tools and their usage note.
let agent = harness
    .create_agent(Agent::new("assistant-v1").instructions("Be helpful."))
    .await?;
let session = agent.new_session().await?;
session.run("Remember that I prefer dark mode.").await?;
session.close().await?;      // sends what is pending and ends the session on HeliconSearch
harness.shutdown().await?;   // always: it sends what is still pending and stops the engine
```

### Options

| Option | Default | Meaning |
| --- | --- | --- |
| `baseUrl` | required | HeliconSearch address. |
| `apiKey` | required | Tenant token: the token itself, or `"${HELICON_TENANT_TOKEN}"`. |
| `agentId` / `projectId` | `openworker` / `""` | Where memories are written and read (strings: anything else is refused). |
| `autoRecall` | `true` | Recall into every turn. |
| `autoCapture` | `true` | Archive and extract after every turn. Off: no new turn is archived or extracted, whoever asks (close and shutdown too). |
| `captureMode` | `operator` | `operator`: HeliconSearch's model extracts, merges and updates facts from the whole conversation. `direct`: no model; user sentences that match a few keyword rules (and `customTriggers`) are stored as they are (at most 3 per capture cycle, normally one turn), and HeliconSearch still extracts the archived conversation when the session ends, so facts can be stored twice. |
| `customTriggers` | none | Extra words that make `direct` mode store a sentence. |
| `sessionBridge` | `true` | Add the cross-session summary to a new session's first turn. |
| `hookTimeoutMs` | `15000` (1000–120000) | The longest a turn waits for memory before it goes on without it. |
| `timeoutMs` | `10000` (1000–120000) | Timeout of ordinary HeliconSearch calls (recall, the operator and summaries have their own). |
| `endOnShutdown` | `true` | `Harness::shutdown` ends the sessions this run worked on and did not close. |
| `idleEndMs` | `1800000` (1000–86400000) | A session with no activity for this long (ms) is ended; a session an earlier run left open is ended at the next start (or once it has been quiet that long). A session with a turn in flight is never idle (an approval, a slow tool or a slow model can keep it quiet for longer). `0`: never. |
| `syncTranscript` | `true` | `false`: sessions are archived but never ended on HeliconSearch (no session summary). |
| `summaryOnSessionEnd` | `true` | After ending a session that HeliconSearch did not summarise itself, ask it for the summary. |
| `agents` | all | Ids of the agents (`Agent::new("<id>")`) that get memory. Every other agent of the harness is left alone: its sessions are neither recalled into nor archived. |
| `closeWaitMs` | `0` (0–300000) | How long `close()` waits for HeliconSearch's answer to this session's extraction request (see below). |
| `replaceBuiltinMemory` | `true` | Agents get the HeliconSearch tools instead of the engine's built-in `memory_*` tools. |
| `memoryStoreTool` | `false` | Also give agents a `memory_store` tool. |
| `topK` | `5` (1–50) | Default number of results of `memory_recall`. |
| `recallTopK` | `12` (1–20) | Memories requested for the automatic recall of a turn. |
| `recallMinScore` | `0.01` (0–1) | Floor below which a recalled fact is not shown. |
| `recallMaxChars` | `1000` (100–10000) | The user's message is cut to this many characters for the recall query. |
| `recallGate` / `storeGate` | `true` / `true` | Switch on HeliconSearch's server-side RecallGate (automatic per-turn recall only; explicit searches always retrieve) and StoreGate (for the extraction of captured turns). |
| `captureMaxChars` | `500` (100–10000) | Longest sentence `direct` mode stores. |
| `transcriptMaxCharsPerTurn` | `20000` (100–50000) | Longest archived turn; longer ones are cut with a marker. |
| `sessionBridgeEpisodicLimit` / `PinnedMinImportance` / `PinnedMinRecall` / `PinnedTopK` / `ProcedureLimit` / `TokenBudget` | `7` / `0.85` / `5` / `20` / `5` / `2500` | What the first-turn summary contains and how much of the prompt it may take (all keys start with `sessionBridge`). |
| `debug` | `false` | Log every request to `<state_dir>/helicon-memory/adapter.log` (warnings are always logged). |

## What to know

- **Memory never blocks or fails a turn.** If HeliconSearch is slow, down or returns an error, the turn is sent without
  recalled memory (a turn waits at most `hookTimeoutMs` for it). Whatever could not be archived is sent the next time
  that session has activity, or when the session is resumed in a later run.
- **Tool calls are approvals.** The memory tools are ordinary application tools, so the agent's approval mode applies:
  with the default `Approvals::Always` the user is asked before the agent searches or forgets. Recall and capture are
  not tool calls and need no approval. (An application names a tool as `helicon.memory_forget`, as in
  `interrupt_on`; the model sees `helicon__memory_forget`.)
- **Forgetting is two steps.** `memory_forget` with a `query` only lists the candidate memories; the agent deletes one by
  calling it again with that memory's `memoryId`. A search score says how well a memory matched, not that it is the one
  meant, and a deletion cannot be undone.
- **Tool results are cut at 60,000 bytes** (a long conversation page, say), with a note saying so.
- **Agent definitions change.** When memory is on, `Harness::create_agent` adds the tool names and a short usage note to
  the agent's `tools` and `instructions` (and turns its `memory` off). An agent id is immutable, so these settings are
  part of the agent: changing `memoryStoreTool`, `autoRecall`, `autoCapture`, `replaceBuiltinMemory` or `agents` in the
  file, turning memory on for an agent that exists without it, or upgrading the SDK (the usage note may be reworded)
  makes `create_agent` fail with a definition conflict for that id. The SDK says so on stderr; register the agent under
  a new id (`assistant-v2`). Sessions of an agent registered before memory existed still get recall and capture, only
  not the tools.
- **Existing sessions are back-filled.** A session that predates the configuration is archived from its first message
  the next time it has a turn.
- **Ending is not final.** A session that was ended and then gets another turn is archived into the same
  HeliconSearch session and ended again when it goes quiet again.
- **`close()` does not wait for facts to exist unless you ask.** It sends what is pending (the verbatim archive) and
  ends the session; HeliconSearch extracts facts and writes the summary on its own. `closeWaitMs` makes `close()` wait
  (up to that long) for HeliconSearch's answer to this session's extraction request. HeliconSearch answers when the
  extraction is done, unless it is still busy with that session's earlier work: then it finishes in the background and
  answers at once. Set it only for a short-lived program that wants its facts available right after it closes.
- **Recalled memory is data, not instructions.** Both the recall and the first-turn summary are added with a notice
  saying so, and escaped.
- **Short messages are not recalled for.** A message under 5 characters gets no recall (a new session's first turn
  still gets its cross-session summary).
- **Everything said is sent to HeliconSearch** and stored there as it was said (a turn longer than
  `transcriptMaxCharsPerTurn` is cut, with a marker), with the facts extracted from it. Tool calls and their results are
  archived too, except the memory tools' own. With `autoCapture: false` no new turn is sent (a session archived earlier
  is still ended).
- **The first-turn summary is headed in Chinese** (`跨会话记忆` and its sections), the same text as the TypeScript SDK's
  adapter.
- **A retry gets the same text.** The recall for one submission is remembered (for the last 256 submissions), so
  resending a submission id (after `Error::OutcomeUnknown`) sends the same turn context. The memory is kept in process:
  after an application restart, or once 256 other submissions have gone by, a resend recalls again, and if the answer
  differs the engine refuses the id as a changed turn context — resend with a new submission id.
- **Guardrails see the recalled text.** The engine runs its safety hooks over the whole turn addendum, so a guardrail
  that blocks on a remembered sentence blocks that turn, every time that sentence is recalled.
- **A session whose history could not be archived (HeliconSearch down) is not ended on HeliconSearch.** While it is
  open, the idle sweep (once it has been quiet for `idleEndMs`) and `Harness::shutdown` try again. One that was closed
  meanwhile is tried again at the next start of the application, once it has been quiet for `idleEndMs`.
- **A `close()` the engine refuses** (a turn is still running: cancel it first) leaves the session open, and it is
  followed again, so that turn is archived.
- **Call `Harness::shutdown`.** It sends what is pending and ends the sessions still open, for at most 45 seconds in all
  (what is left waits for the next start). A harness that is dropped without it still stops its engine, but pending
  work waits for the next start.

## Limits

- **Embedded and managed hosts only.** A `Harness::connect` service client has no extension hooks.
- **Agents given to `HarnessBuilder::agent` are compiled by the host before application tools can be registered**, so
  they do not get the memory tools or the usage note; their built-in memory is still turned off and their sessions are
  still recalled into and archived. Use `harness.create_agent` for agents that should have the tools. (If the extension
  cannot start, the reason is printed to stderr and the built-in memory of those agents stays off.)
- Recall covers the turns you send through a `Session`. Sub-agents and team workers started by the engine are neither
  recalled into nor captured (only the session you created or resumed is); turns the engine starts by itself (timers)
  in such a session are archived like any other, but are not given recalled memory.
- The memory tools are registered with the engine under fixed texts, and the engine keeps what it was given for a tool
  name. An SDK upgrade that has to reword one of them cannot re-register it in a state directory that already holds the
  old one: the extension stays off and says so on stderr (`Application tool definition changed`). No option changes a
  tool's text, so changing options never causes this.
- A session an earlier run left open is ended at the next start, at most 10 per start (a summary queue is not flooded);
  a session with a turn in flight is not called idle for up to 6 hours.
- Tool results are text. Images and documents a user attaches are archived as placeholders, not their bytes.
- A run that is killed (no `close()`, no `shutdown()`) has its open sessions ended at the next start once they have
  been quiet for `idleEndMs`, not at once.
- Sessions of an application that never closes them stay known (a few bytes each) until they are ended by the idle
  sweep; the progress files in `<state_dir>/helicon-memory/sessions/` are not removed.

## The extension seam

`openworker_sdk::extension::Extension` is a public trait with default no-op hooks. `HarnessBuilder::extension` attaches
your own. With no extension attached every hook is skipped and the SDK behaves exactly as before. Hooks are advisory: a
hook that panics (while it builds its future or while it runs) is ignored and reported on stderr (when panics unwind:
with `panic = "abort"` a panic ends the process), an addendum the engine would refuse (over 32,000 bytes, or with a NUL)
is ignored, and an `on_start` that fails or panics drops that extension and the harness starts without it. Nothing
bounds a hook's time: a slow hook delays the call it hooks into and one that never returns holds it for ever, so a hook
bounds its own time. Several extensions are called in the order they were added, each seeing the addendum the one
before made. See `tests/extension.rs` for extensions of the test's own.

| Hook | Called |
| --- | --- |
| `on_start(StartContext)` | when the harness is up: register tools (`Harness::register_tools`), subscribe to the journal |
| `decorate_agent(&mut Agent, &DecorateContext)` | before an agent is registered or compiled: add tools, text, turn the built-in memory off |
| `on_session_created`, `on_session_attached` | when a `Session` is created, or created or reattached (its journal is now delivered) |
| `before_send(TurnContext) -> Option<String>` | before each message is sent: return the `instruction_addendum` to use (the caller's own is in `turn.addendum`; keep it first). `submission_id` is stable for a retry |
| `before_close` | when `Session::close` is called |
| `on_close_failed` | when that `close` failed (the engine refused it): the session is still open |
| `on_shutdown` | when `Harness::shutdown` is called |

`before_send` only runs when the host reports `turn_context` (every current host does). Every way of sending a text
message calls it once: `run`, `run_streamed`, `run_with`, `run_with_submission`, `run_input`, `run_input_streamed`,
`send`, `send_with_id`, `send_with_limits`, `send_with_options` and `send_input`. (Raw `HarnessClient` calls and
`Harness::connect` service sessions bypass the hooks.)

## Troubleshooting

Look at `<state_dir>/helicon-memory/adapter.log` (warnings are always logged there), and set `"debug": true` to see
each request and the injected recall block. Per-session progress is in
`<state_dir>/helicon-memory/sessions/<session id>.json`; deleting a file makes the next turn of that session send its
whole history again (HeliconSearch ignores what it already holds).

| Symptom | Likely cause |
| --- | --- |
| `openworker: helicon-memory is off: ... apiKey is missing` on stderr | the token variable named in `apiKey` (`${HELICON_TENANT_TOKEN}`) is not set in the process environment |
| `openworker: helicon-memory is off: ... file not found` | `OPENWORKER_HELICON_CONFIG` points at a file that does not exist |
| Nothing is recalled, the log has no `adapter on:` line | no `helicon-memory.json` in the state directory the harness really uses (`HarnessBuilder::state_dir`), `"enabled": false`, or `debug` is off (that line is only logged with `debug`) |
| The agent never calls the memory tools | it was registered with `HarnessBuilder::agent` (no tools there): use `harness.create_agent` |
| `This profile requires a runner` when creating the agent | the memory tools are tools: build the `openworker-harness` helper and pass `.executable(path)` or set `OPENWORKER_EXECUTABLE` |
| `create_agent` fails with a definition conflict after a configuration change | see "Agent definitions change": register the agent under a new id |
| `openworker: extension helicon-memory is off: Application tool definition changed` | the state directory holds an older text of a memory tool (see Limits): use a fresh state directory |
| `status=401` in the log | the token was rejected (revoked, or its tenant disabled) or missing |
| `HeliconSearch not reachable` in the log | wrong `baseUrl`, server down or a proxy in between; turns still work, without memory |
| `HeliconSearch not reachable` although `curl --noproxy "*"` reaches it | the process has `HTTP_PROXY` / `HTTPS_PROXY` set and the HeliconSearch host is not in `NO_PROXY` (calls go through the proxy, except to a loopback address): add it to `NO_PROXY` |
| A fact told a moment ago is not recalled | HeliconSearch extracts facts with a model, which takes seconds to minutes: it is recalled once that has happened |
| Recall is slow | each turn waits at most `hookTimeoutMs` (15 s) for memory, then goes on without it |

## Tests

From the `harness/` directory:

```sh
cargo test -p openworker-sdk --lib helicon                                    # unit tests; a fake HeliconSearch on loopback and answers recorded from a real one
cargo test -p openworker-sdk --features test-support --test helicon           # real engine, scripted model, fake HeliconSearch
cargo test -p openworker-sdk --features test-support --test helicon_model     # real engine, fake model: what the model is actually sent
cargo test -p openworker-sdk --features test-support --test extension         # the Extension seam, with extensions of the test's own
# against a real HeliconSearch (a throw-away agent id; it removes the memory it stores, the archived test session
# and the summary HeliconSearch wrote for it stay). The feature keeps these tests out of every other run:
HELICON_LIVE_URL=http://127.0.0.1:5001 HELICON_LIVE_TOKEN=hs_... \
  cargo test -p openworker-sdk --features helicon-live-test --lib live_memory -- --nocapture
# read-only, against an agent that already has memories (its cross-session summary and recall):
HELICON_LIVE_URL=... HELICON_LIVE_TOKEN=... HELICON_LIVE_READ_AGENT=<agent id> \
  cargo test -p openworker-sdk --features helicon-live-test --lib live_the_bridge -- --ignored --nocapture
```
