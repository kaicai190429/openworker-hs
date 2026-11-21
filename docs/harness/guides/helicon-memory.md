# Long-term memory and media search with HeliconSearch

The TypeScript SDK can give your agents long-term memory and a searchable library of the user's images and
videos, both served by
HeliconSearch. It is off until you
add one configuration file. Your application code does not change: the same `Harness.start`, `newSession`,
`run`, `send` and `close` calls apply.

With it on:

- **Recall.** Before each message is sent, memories relevant to it are retrieved and added to that turn's
  `instructionAddendum`. The addendum is not part of the saved conversation, so recalled text is never stored again.
- **A new session's first turn** also gets a short cross-session summary (recent sessions, pinned facts,
  project workflows). It is shown once, on that first turn, and not repeated.
- **Capture.** After each completed turn, the new conversation is archived verbatim on HeliconSearch and facts
  are extracted from it.
- **Memory tools.** Agents get `helicon__memory_recall`, `helicon__memory_get`, `helicon__memory_forget` and
  `helicon__skill_get` (and `helicon__memory_store` if you enable it).
- **Media tools.** Agents get `helicon__media_search` and `helicon__media_upload` (see below).
- **Session summary.** A session is ended on HeliconSearch, which then writes its summary, when you call
  `session.close()`, when you shut the harness down, when it has been idle for 30 minutes, and, for a run that
  died, at the next start.

## Quick start

You need a HeliconSearch server you can reach (by default `http://127.0.0.1:5001`) and a tenant token for it.

```sh
# 1. administrator, once: register a tenant and write the configuration (the token stays in an environment variable)
HELICON_TENANT_ADMIN_PW=<administrator password> \
  node scripts/helicon-provision.mjs --url http://hs-host:5001 --tenant my-app --env HELICON_TENANT_TOKEN
# 2. put the file it wrote (./helicon-memory.json) in your harness stateDir, and give the process the token
export HELICON_TENANT_TOKEN=hs_...
# 3. run your application as usual
```

Register agents with `harness.createAgent` (they then get the memory tools). To check that it works, set
`"debug": true` and read `<stateDir>/helicon-memory/adapter.log`: an `adapter on:` line at start, then per turn the
recall (`auto_recall injected ...`) and what was archived (`archived turns=...`). If memory is off for a reason (a bad
file, no token) the harness still starts and Node prints `helicon-memory is off: ...`. Facts are extracted by
HeliconSearch's model, which takes seconds to minutes: a fact told in one session is recalled from others once that has
happened.

## Turn it on

Create `helicon-memory.json` in the harness `stateDir` (or point `OPENWORKER_HELICON_CONFIG` at a file
elsewhere; a packaged Electron application can also ship it in its resources directory):

```json
{
  "baseUrl": "http://127.0.0.1:5001",
  "apiKey": "${HELICON_TENANT_TOKEN}",
  "agentId": "my-app",
  "captureMode": "operator"
}
```

`${NAME}` is read from `process.env` when the harness starts, so the token need not be in the file. Without a
token the memory stays off and Node prints a warning; no file, or `"enabled": false`, means memory is off and
the SDK is untouched.

All memories belong to the tenant the token identifies. `agentId` and `projectId` are partitions inside that
tenant: everyone who shares a token shares its memories and media.

### Shipping an application: no tenant for users to create

HeliconSearch refuses every memory call without a tenant token, so an application that uses it needs one. Create
it once, as the person who deploys the application, and ship it inside the configuration:

```sh
HELICON_TENANT_ADMIN_PW=<administrator password> \
  node scripts/helicon-provision.mjs --url http://hs-host:5001 --tenant my-app --out helicon-memory.json
```

The script (it needs only Node) registers the tenant, or issues a new token for an existing one, checks that the
token works, and writes the configuration. Put the file in your application's resources
(`<resourcesPath>/helicon-memory.json`) or its state directory, and users configure nothing. Keep the file out
of source control; the token it holds gives access to that tenant's data. To keep the token out of the file, run
with `--env HELICON_TENANT_TOKEN` and supply the variable to the application.

### Options

| Option | Default | Meaning |
| --- | --- | --- |
| `baseUrl` | required | HeliconSearch address. |
| `apiKey` | required | Tenant token: the token itself, or `"${HELICON_TENANT_TOKEN}"`. |
| `agentId` / `projectId` | `openworker` / `""` | Where memories are written and read (strings: anything else is refused). |
| `autoRecall` | `true` | Recall into every turn. |
| `autoCapture` | `true` | Archive and extract after every turn. Off: nothing of the conversation is sent, not even when a session is closed or the harness shut down. |
| `captureMode` | `operator` | `operator`: HeliconSearch's model extracts, merges and updates facts from the whole conversation. `direct`: no model; user sentences that match a few keyword rules are stored as they are (at most 3 a turn), and HeliconSearch still extracts the archived conversation when the session ends, so facts can be stored twice. Use `direct` only when HeliconSearch has no extraction model. |
| `sessionBridge` | `true` | Add the cross-session summary to a new session's first turn. |
| `hookTimeoutMs` | `15000` | The longest a turn waits for memory before it goes on without it. |
| `endOnShutdown` | `true` | `harness.shutdown()` ends the sessions this run worked on and did not close. |
| `idleEndMs` | `1800000` | A session with no turn for this long (ms) is ended; a session an earlier run left open is ended at the next start (or once it has been quiet that long). A session with a turn in flight is never idle. `0`: never. |
| `agents` | all | Names of the agents that get memory. Every other agent of the harness is left alone: its sessions are neither recalled into nor archived. |
| `closeWaitMs` | `0` | How long `close()` waits for this process's own fact extraction (`0`: not at all; see below). |
| `replaceBuiltinMemory` | `true` | Agents get the Helicon tools instead of the engine's built-in `memory_*` tools. |
| `memoryStoreTool` | `false` | Also give agents a `memory_store` tool. |
| `autoMedia` | `true` | Give agents the media tools. |
| `mediaAdminTools` | `false` | Also `media_search_by_image`, `media_libraries`, `media_files`, `media_statistics`. |
| `mediaInlineImages` | `false` | `media_search` also shows the best image hit to the model (needs a vision model). |
| `mediaUploadRoots` | `[]` | Absolute folders `media_upload` may add files from (at most 50; `${VAR}` is substituted). Empty: it refuses every path the agent names. See "Media search and upload". |
| `mediaMaxResults`, `mediaMinSimilarity`, `mediaKbId`, … | see `config.ts` | Media search tuning and a fixed target library. |
| `recallTopK`, `recallMinScore`, `recallMaxChars`, `recallGate`, `storeGate` | see `config.ts` | Recall and extraction tuning, as in the OpenClaw plugin. |
| `debug` | `false` | Log every request to `<stateDir>/helicon-memory/adapter.log` (warnings are always logged). |

## Media search and upload

`media_search(query)` searches every media library of the tenant and returns the best matches, one entry per file:

```json
{ "results": [ { "rank": 1, "file": "C:/photos/blue.png", "type": "image", "similarity": 0.84 },
               { "rank": 2, "file": "/clips/a.mp4", "type": "video", "similarity": 0.62, "startSeconds": 3, "endSeconds": 9.5 } ] }
```

The result is in the tool's journal event, so your application can show the pictures itself; the agent is told
to give the user the paths of the matches it recommends. With `mediaInlineImages`, the best image is also put in
front of the model as a picture (one JPEG or PNG, as the SDK's tool images allow).

`media_upload(file_names?)` adds files to the tenant's default library (HeliconSearch creates it when there is
none). Paths are paths **on the machine HeliconSearch runs on**, which reads the files itself. When the user
attaches images to a message and asks for them to be kept, the agent calls it with no paths: the SDK saves the
attached images under `<stateDir>/helicon-memory/uploads/` and adds those. Indexing takes tens of seconds; files
are searchable when it finishes (`media_statistics` shows progress).

Audio and video cannot be attached to a message in the SDK, so only files already on disk (by path) can be added.

### Which paths `media_upload` accepts

HeliconSearch indexes whatever it is given (images, videos and documents such as PDF or Excel, a whole folder
included), as its own OS user, and `media_search` then returns what it found. So a path the agent names is only
passed on when it is inside a folder you list in `mediaUploadRoots`; the default is none, and `media_upload`
then answers that uploading is not enabled and does nothing.

```json
{ "mediaUploadRoots": ["/data/media", "${MEDIA_SHARE}/clips"] }
```

- Every entry is an absolute path (checked when the configuration is read; the folder need not exist yet).
- The files, or folders, the agent names must equal an allowed folder or lie inside it, by whole path components
  (`/data/media-evil/x` is not inside `/data/media`). Symbolic links in the part of the path that exists on the
  machine the adapter runs on are resolved, so a link inside an allowed folder that points out of it does not lead
  out. On Windows the comparison ignores case and accepts `/` and `\`.
- A path with a `..` segment is refused, even where it would stay inside the folder (a link followed by `..` goes
  somewhere else than the text says). A `..` in an entry of `mediaUploadRoots` itself is resolved: `/data/media/trip/..`
  is `/data/media`.
- All or nothing: if one of several paths is outside, none is added, and the agent is told only that and how many
  were outside, not which folders are allowed. The refusal is also written to the adapter log.
- The images the user attached to a message are not affected: with no `file_names` the agent adds the images the
  adapter saved itself, whatever `mediaUploadRoots` holds.
- The path is passed to HeliconSearch as the agent gave it. The check happens on the machine the adapter runs on,
  and HeliconSearch opens the path later: when HeliconSearch runs on another machine the check can only compare the
  text of the path (nothing there can be resolved), and a file swapped for a link after the check is not caught.
  It is a guard against the model naming a path, not a sandbox. Allow only folders HeliconSearch may read for
  this tenant.
- The tool's description and the agent's usage note do not change with this option (they are stored with the agent
  definition), so the explanation is in what the tool answers.

## What to know

- **Memory never blocks or fails a turn.** If HeliconSearch is slow, down or returns an error, the turn is sent
  without recalled memory. Whatever could not be archived is sent the next time that session has activity, or
  when the session is resumed in a later run.
- **Tool calls are approvals.** The memory and media tools are ordinary application tools, so the agent's
  approval mode applies: with the default `always`, the user is asked before the agent searches, uploads or
  forgets. Recall and capture are not tool calls and need no approval.
- **Forgetting is two steps.** `memory_forget` with a `query` only lists the candidate memories; the agent deletes one
  by calling it again with that memory's `memoryId`. A search score says how well a memory matched, not that it is
  the one meant, and a deletion cannot be undone.
- **Tool results are cut at 60,000 characters** (a long conversation page, say), with a note saying so.
- **Agent definitions change.** When memory is on, the SDK adds the tool names and a short usage note to each
  agent's `tools` and `instructions` at registration. Agent definitions are immutable per name, so turning
  memory on or off for an existing agent needs a new agent name.
- **Existing sessions are back-filled.** A session that predates the configuration is archived from its first
  message the next time it has a turn.
- **A session whose history could not be archived (HeliconSearch down) is not ended on HeliconSearch.** While it is
  open, the idle sweep (once it has been quiet for `idleEndMs`) and shutdown try again. One that was closed
  meanwhile is tried again at the next start of the application, once it has been quiet for `idleEndMs`.
- **A `close()` the engine refuses** (a turn is still running: cancel it first) leaves the session open, and it is
  followed again, so that turn is archived.
- **Ending is not final.** A session that was ended and then gets another turn (resumed later, or after an idle
  ending) is archived into the same HeliconSearch session and ended again when it goes quiet again.
- **`close()` does not wait for HeliconSearch to finish.** It sends what is pending (the verbatim archive) and ends
  the session; HeliconSearch extracts facts and writes the summary on its own, usually within a minute, and
  longer when its queue is busy. Facts the running process was already extracting continue while it lives.
  `closeWaitMs` makes `close()` wait (up to that long) for HeliconSearch's answer to this session's extraction
  request: HeliconSearch answers when the extraction is done, unless it is still busy with that session's earlier
  work, then it finishes in the background and answers at once. Set it only for a short-lived script that wants
  its facts available right after it closes.
- **Recalled memory is data, not instructions.** Both the recall and the first-turn summary are added with a notice
  saying so, and escaped.
- **A retry gets the same text.** The recall for one submission is remembered, so resending a submission id (after
  an unknown outcome) sends the same turn context. The memory is kept in process: after an application restart a
  resend recalls again, and if the answer differs the engine refuses the id as a changed turn context; resend with
  a new submission id.
- **Guardrails see the recalled text.** The engine runs its safety hooks over the whole turn addendum, so a
  guardrail that blocks on a remembered sentence blocks that turn, every time that sentence is recalled.
- **Everything said is sent to HeliconSearch** and stored there verbatim, with the facts extracted from it.
  Tool calls and their results are archived too, except the memory and media tools' own.

## Safety

- **Approve the tools that act.** `media_upload` makes HeliconSearch read files you name from its own disk, and a
  model can be talked into naming the wrong ones (a directory included; `mediaUploadRoots` limits where); `memory_forget` deletes. With the default
  approval mode the user is asked each time. If you turn approvals off for an agent, think about these two first.
- **`media_upload` adds nothing from outside `mediaUploadRoots`.** The default is no folder at all. List only
  folders whose contents you are willing to have indexed and read back through `media_search`; never a home
  directory or a drive root, and not a folder with documents you would not show the agent.
- **Memory is shared by everyone who shares the token.** One tenant, one pool of memories and media.
- **Only `agents` named in the configuration are touched**, so an application with several agents can keep, say, a
  coding agent's conversations out of HeliconSearch.
- **`mediaInlineImages` reads files at the paths HeliconSearch reports.** Leave it off unless that server is trusted.
- **Attached images are written to `<stateDir>/helicon-memory/uploads/`** (only while media tools are on) so
  HeliconSearch can index them, and are deleted after 7 days. The adapter log is readable by its owner only.

## Limits

- Agents you pass in `Harness.start({ agents })` are registered before the adapter exists, so they do not get the
  tools or the usage note (their sessions are still recalled into and archived). Create agents with
  `harness.createAgent`.
- Each turn waits for recall before it is sent, at most `hookTimeoutMs` (HeliconSearch normally answers in 1-2 s).
- The memory tools are registered with the engine under fixed texts, and the engine keeps what it was given for a
  tool name. An SDK upgrade that has to reword one of them cannot re-register it in a state directory that already
  holds the old one: the adapter stays off and says so (`helicon-memory is off: ... definition changed`). No option
  changes a tool's text, so changing options never causes this. (A state directory used with an earlier build of this
  adapter, whose `memory_recall` text named `topK` and `autoRecall`, holds the old text: use a fresh state directory.)

- TypeScript SDK with the managed engine process only. A `Harness.connect()` service client does not get it. A Rust
  application uses the Rust SDK's own memory extension (`helicon-memory-rust.md`), which reads the same
  `helicon-memory.json`.
- Recall covers the turns you send through a `Session`. Sub-agents and team members started by the engine, and
  turns the engine starts by itself (timers), are not given recalled memory and are not captured: only the
  session you created or resumed is.
- Images and documents a user attaches are archived as placeholders, not their bytes.
- An application that is killed (no `close()`, no `shutdown()`) has its open sessions ended at the next start, not at once.

## Models behind an HTTP proxy

The engine's model client does not use `https_proxy`. If the machine reaches the internet only through a proxy, the
engine cannot call a cloud model itself; register the provider with a loopback `http://127.0.0.1:<port>/v1` base URL
and run a small relay on that port that forwards through the proxy (`test/helicon-relay.mjs` is one). HeliconSearch
calls are made by the SDK process, which is not affected.

## Troubleshooting

Look at `<stateDir>/helicon-memory/adapter.log`, and set `"debug": true` to see each request. Per-session
progress is in `<stateDir>/helicon-memory/sessions/<session id>.json`; deleting a file makes the next turn of
that session send its whole history again (HeliconSearch ignores what it already holds).
