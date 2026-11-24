# Agent chat and tool evidence

## First implementation

The Playground offers Chat alongside Terminal and Browser. Chat launches the selected external executable inside the traced sandbox and permits one foreground task at a time. Codex continues conversations through `codex exec resume`. AgentKit keeps one Go process with an in-memory conversation per sandbox.

Each Codex session pins `model = "gpt-6-sol"` and `model_reasoning_effort = "high"` in its sanitized `config.toml` and named `.config.toml` copies. Terminal Codex uses the session configuration. The adapter also explicitly passes these settings for Chat and Security tests, including resumed turns.

Codex model semantics come from observed Responses HTTP requests, JSON responses, and SSE or WebSocket messages. The collector reads allowlisted correlation environment variables at successful exec. Codex session and rollout files are excluded from file-content previews. The launcher uses the CLI stdout `thread.started` control notification to retain the conversation ID.

Model and tool records are grouped using transport request IDs and Responses IDs, including successive responses on one WebSocket connection. Process attribution retains its separate inferred provenance.

```mermaid
flowchart LR
    Chat[Chat page] --> WS[Owner WebSocket]
    WS --> Collector[Collector control channel]
    Collector --> Workspace[Traced workspace parent]
    Workspace --> Shell[Terminal / Chrome session]
    Workspace --> Codex[External Codex]
    Workspace --> AgentKit[External AgentKit]
    AgentKit --> Proxy
    AgentKit --> Tools
    Codex --> Tools[Tool subprocesses]
    Codex --> Proxy[Built-in HTTP / TLS proxy]
    Tools --> Proxy
    Proxy --> Egress[Host egress and credential injection]
    Egress --> Model[Model endpoint]
    Egress --> Resources[Other HTTP resources]
    Proxy --> Semantic[Responses or Chat Completions parser]
    Semantic --> Payloads[Complete semantic payload store]
    Semantic --> Journal[Shared event journal]
    Tools --> Tracer[Process and file collectors]
    Tracer --> Journal
    Journal --> Views[Chat, trajectory, independent monitors]
```

## Installation and configuration

The server discovers the native Codex distribution through PATH or an npm installation under `/opt/nvm`. The adapter mounts the executable, Code Mode helper, package manifest and installed runtime resources at their package-relative paths. Each bind is read-only. A binary in a shared directory such as `~/bin` exposes only the listed components.

For each session, the server reads `config.toml`, named `.config.toml` profiles, `auth.json` and `models_cache.json`, when present, from the external `CODEX_HOME`. The Codex adapter replaces login tokens, API keys and configured authentication headers with session placeholders, then audits the resulting files before transferring the bounded snapshot over Docker stdin. Codex receives a private writable home under `/records/codex`, initialized before the terminal starts. Terminal and Chat share this home, the mounted executable path, CA settings and box-owned TCP egress. A host-side gateway injects real authentication only for the session's permitted API operations after traffic has left container capture. The host authentication file is read for each authenticated request. Expired credentials require host reauthentication and an account change requires a new session. The private home, native continuation state, control socket and evidence share container tmpfs and disappear when the sandbox is destroyed. Authentication formats beyond the file-based ChatGPT and API-key adapter require explicit support. The credential boundary and checks are described in [Codex credential isolation](codex-credential-isolation.md).

The `/records` tmpfs explicitly permits execution so interactive Codex can stage and start its managed background server under `/records/codex/packages`. Its `nosuid` and `nodev` mount options remain enabled. Terminal users can run `codex` with its default daemon behavior. Chat uses `codex exec`.

Codex runs with approvals and its additional command sandbox disabled inside the existing AgentTomo Docker sandbox. The AgentTomo tracing, network redirection and temporary CA trust still apply. The existing model choice and provider configuration are reused. HTTP capture is enabled automatically when an external Codex installation is available.

`run --proxy http://host:port` and `playground --proxy http://host:port` configure the box's TCP outbound connector. The connector reuses the existing HTTP Host / TLS SNI inspector on a bounded client preface. A recognized hostname selects `CONNECT hostname:original-port`, allowing the upstream proxy to resolve the name. Missing names, unknown protocols and server-first connections retain `CONNECT original-IP:port`; inspection waits at most 500 ms and buffers at most 64 KiB. Shared replay I/O preserves the inspected bytes. Explicit application proxy CONNECT or absolute-form HTTP requests keep their original proxy endpoint. This does not change DNS configuration or recover a DNS failure that prevents the application from connecting at all. Hidden SNI and prefaces exceeding these limits retain IP routing.

After CONNECT, the connector forwards arbitrary TCP bytes. Original socket destinations, recorded five-tuples and process attribution remain intact. HTTP/TLS content capture is independent, and transport CONNECT headers are not recorded as application requests. With this option, inherited application proxy variables are removed; Codex and its tools use their original destinations. Proxy errors never fall back to direct. This milestone supports unauthenticated HTTP CONNECT upstreams; UDP is unchanged.

A host-loopback proxy is reached through the host entry bound to the Docker bridge. With the Codex adapter active, that entry performs scoped authentication forwarding and relays other TCP connections through the selected upstream. It accepts registered, owned sandbox container addresses and closes with the Playground server. The upstream setting and public gateway CA cross container bootstrap in memory. Hyper's upgraded stream preserves prefetched bytes for server-first protocols and half-close behavior. Container capture and host authentication forwarding reuse the same HTTP transport implementation.

Playground also reads an optional `.agenttomo/playground.json` relative to its working directory. This ignored local file persists the proxy across clean shells.

```json
{"proxy":"http://127.0.0.1:8118"}
```

The explicit `--proxy` option takes precedence. Playground retains host proxy variables and then this file as compatibility fallbacks, but they now configure the box rather than the application. Conflicting nonempty URLs fail with instructions to choose one using `--proxy`. An explicitly empty proxy variable disables the file fallback. With no configured override, the default is `http://127.0.0.1:8118`. `--no-proxy` selects direct box forwarding. A configured box proxy applies to all intercepted TCP, without NO_PROXY routing. Plain `run` uses its explicit option or the same default, without environment/file fallbacks.

```bash
sg docker -c './target/debug/agenttomo-box playground --proxy http://127.0.0.1:8118 --listen 0.0.0.0:18889'
```

CLI stdout supplies thread lifecycle and immediate runtime diagnostics, including `error` and `turn.failed`. These diagnostics update the task while Codex retries, with authentication and connection hints plus expandable original details. Nonzero exits always carry an error message even if stderr is empty. Model and tool contents continue to come from observed traffic. Chat shows the current phase and elapsed time, and permits drafting the next prompt while a task is running.

## Agent and protocol boundaries

Shared launch, tracing, recording, forwarding and UI code consume built-in trait implementations. `agent/adapter.rs` registers `AgentAdapter` implementations by ID. Each adapter supplies display metadata, configuration rules, launch arguments and stdin encoding, runtime rules and lifecycle output parsing. Its host-side `Installation` owns discovery results, the list of read-only runtime mounts and sanitized configuration preparation. Codex flags, filenames, environment setup and control-record parsing remain in `agent/codex.rs`. The registry serializes only the registered ID (`"codex"` remains unchanged); unknown IDs fail instead of silently selecting another adapter.

`agent/workspace.rs` owns process creation, correlation and cancellation. Codex receives one prompt and stdin EOF per invocation. AgentKit runs the official Go HTTP application on sandbox loopback. The workspace creates an SDK session and posts each prompt to `/run_sse` with streaming enabled. A final visible model event followed by a clean end to the AgentKit stream completes the turn. Chat displays the text captured from the outbound model stream. Stop terminates the server and the next prompt starts a new session.

At the executable boundary, the selected adapter supplies runtime executable names to the correlation hub. The collector receives these rules without selecting or depending on Codex. With no adapter selected, ordinary `run` has no implicit Codex runtime classification. Registered adapters' private preview paths remain excluded even in that mode, preserving sensitive-file protection while retaining file-open events. `run` does not discover a host login, snapshot configuration or activate the host credential gateway. Playground assembles those optional integration pieces.

`proxy/credentials` owns the existing host-only gateway, placeholder matching, exact origin/method/path enforcement, session revocation and static or reloadable header sources. `agent/codex/credentials` supplies the allowed operations and interprets Codex login/config formats. Host login reload still checks account consistency and token expiry. Credential sources and policies are never serialized; bootstrap sends only sanitized files. The gateway continues to reuse the common HTTP transport, TLS authority and outbound CONNECT implementation.

`agent/protocol.rs` selects a model protocol using the HTTP method and path. Each request has its own decoder. Responses supports its existing HTTP and WebSocket formats. Streamed Chat Completions captures visible text and function calls, assembles argument fragments by choice and tool index, and closes the model round on `[DONE]`. A round that ends with `finish_reason: "tool_calls"` can complete with no text. The proxy snapshots the foreground turn when each request starts, including requests on a reused connection. Late response fragments keep their original task ID. `agent/wire.rs` applies the shared capture limits and writes normalized events for both protocols.

Adding a built-in adapter requires implementing these contracts and registering it, rather than adding Agent-specific branches to the collector, workspace or UI. This is compile-time integration, without dynamic library loading or a separate plugin framework. Tests exercise the common launcher with a non-Codex input format and exercise separate decoders with non-JSON payloads.

Session metadata now includes `agent.id`, `agent.name`, `agent.avatar` and `agent.authentication_hint`, alongside the existing `agent_available` flag. Owner and monitor pages use the same metadata. UI code retains existing cards, evidence links and disclosure state, with a generic Agent fallback when metadata is absent.

AgentKit's host adapter mounts only its Go executable. Its session configuration carries the model name, base URL and a random credential placeholder. The host gateway injects the selected Ark credential after sandbox capture, scoped to the configured origin and chat endpoint. The Go example registers a local command function through ADK, with tool calls and returns captured by the Chat Completions decoder. Available agents share the sample preparation and artifact verification path. Each AgentKit sample starts a separate process and SDK session, while ordinary Chat retains its server and conversation. The report sample applies the same syscall filter used by the command-line launcher. See the [AgentKit configuration](../crates/agenttomo-box/README.md#配置火山引擎-agentkit).

## Events and retained contents

The event category is `agent`. Records include a task ID, Agent instance ID, HTTP request ID and capture timestamp when applicable.

Complete `agent.message` events include the protocol-independent `message_kind`: `intermediate`, `final` or `unknown`. Each protocol decoder determines this value; the Responses decoder maps `commentary` and `final_answer`, while missing or unrecognized phases remain `unknown`. The original `phase` field is retained as evidence. The frontend uses the normalized kind for answer placement and trajectory deduplication. Unknown messages use the existing completed-task/last-message fallback; a still-running or interrupted task does not imply a final answer.

For compatibility, the frontend converts old events without `message_kind` once when they enter the task projection, preserving their previous phase-based behavior. New events take precedence over raw phase, including an explicit `unknown`. Model cards display the observed model name or `Unknown model`; a missing model name never implies a specific provider or protocol.

| Logical record | Raw updates | Evidence |
| --- | --- | --- |
| Task | `agent.task.started`, `agent.task.diagnostic`, `agent.task.finished`, `agent.task.rejected` | Submitted prompt, live runtime diagnostics and execution outcome |
| Model call | `agent.model.request`, `agent.model.started`, `agent.model.complete` | Decoded request, response ID, usage and status. Responses retains the final object. Chat Completions stores the visible choices assembled from the stream and identifies the source in the saved payload. |
| Tool call | `agent.tool.started`, `agent.tool.call`, `agent.tool.result` | Complete model-visible parameters and returned content |
| Assistant message | `agent.message.delta`, `agent.message` | Streaming text, refusals and complete final message |
| Capture issue | `agent.capture.error` | Unsupported, malformed, incomplete or oversized protocol data |

`agent-payloads.bin` stores complete JSON-encoded semantic values. Journal records reference byte ranges. The authenticated `agent-payload` endpoint applies the same session-directory and no-symlink checks as existing payload endpoints. The network preview prefix and stream sliding window do not determine semantic retention. Explicit JSON responses retain their HTTP body and model outcome, including error responses. A successful Responses exchange without a content type retains the existing SSE fallback. Forwarding preserves the original bodies.

Responses custom tools include Code Mode source text. Function tools retain the argument string as sent by the model. A truncated Chat Completions choice keeps its partial arguments in the model detail and has an incomplete status. In later Chat Completions requests, tool messages following the latest user message supply the model-visible return value. Both protocols join returned content to the call by ID and deduplicate repeated results across requests. Native or provider-side tool records are retained when present in Responses output. Returned fields are shown when the protocol exposes them.

Code Mode can call multiple inner tools and choose which values to return to the model. The outer call's observed input and output are available. Inner outputs omitted from the model protocol remain unavailable. A missing return is explicitly shown in the tool inspector.

## Process association

The core identity is `(PID, process_instance_id)`. AgentTomo tool IDs are generated independently of model call IDs. Agent-specific execution IDs are not collected or used for process association.

1. The launcher seeds `AGENTTOMO_AGENT_ID` and `AGENTTOMO_TASK_ID` in the Codex process environment.
2. At successful exec, the tracer reads only these identifiers and `AGENTTOMO_TOOLCALL_ID` from the environment.
3. An explicit AgentTomo tool ID is used when supplied. Otherwise a new worker binds to the active model-visible call at process birth, or at its first applicable exec. Fork-only workers are covered.
4. The binding is stored in the tracer's shared runtime map and retained across later execs. Forked processes inherit it.
5. Codex runtime executables retain task-level bindings without a tool assignment. This executable-role rule remains in the Codex adapter: a persistent tool host must not keep its first tool binding and pass it to workers from later calls. Timing alone cannot reliably distinguish a runtime helper from a worker.
6. Network and file records acquire their process's binding. Shared socket ownership remains subject to the existing network attribution rules.

The active fallback is released when the corresponding model-visible tool return is observed. Existing process bindings and their descendant inheritance continue to identify that tool.

Stopping a task uses pinned Linux pidfds to signal its observed processes and catches later descendants in the same task scope. The terminal remains available. A process that changes to an identity the collector cannot signal produces a visible error and remains traced.

Bindings use `explicit_environment`, `inferred_process_birth`, `inferred_active_call`, `inherited_explicit`, `inherited_inferred` or `agent_runtime` provenance. The fallback does not inject a tool ID into the target's environment. Its frozen process map provides descendant inheritance.

An older yielded Code Mode cell may start a new process after a newer call becomes active. Initial fallback attribution can then be ambiguous. Inferred and inherited-inferred edges remain visibly marked. Processes without a tool binding appear directly under their task, preserving process parentage and resource children, including internal network requests whose exact tool ownership is unavailable.

## Views and limits

The trajectory is a collapsible tree of tasks, model calls, tools, processes and resource cards. TCP flows contain HTTP exchanges. Existing network aggregation, filters, file preview and the raw event inspector are reused. Process nodes start collapsed. Tool details show complete request parameters and returned content in peer sections, each with collapse and copy controls.

Chat and trajectory presentation retain DOM nodes by stable IDs. Live updates reconcile content and order without replacing pressed/focused controls. Trajectory cards show a left disclosure arrow only when they have visible children; empty cards reserve the same space to keep text aligned, and show the arrow when children arrive. The right evidence inspection icon remains available independently of child events, and is disabled until an underlying event is available. Clicking an expandable card or its arrow toggles children. Final-answer message nodes are omitted from the trajectory, and their complete response remains accessible through the model call inspector. Chat uses consistent headers and one assistant card with the execution/status/error disclosure below the answer. Elapsed time is shown once in that footer. A footer without intermediate content, progress or errors retains its status and elapsed time without an expand control. Chat fills the available panel width with compact side padding. User cards remain right-aligned, while their avatar and You label align to the left inside the header.

Sibling nodes are ordered by their recorded start time, with journal sequence breaking ties. Messages retain their first observed time as later deltas and completion update the same node. Model calls are numbered chronologically as `Model call N`, including requests with `generate:false`. Selecting a model call shows its request and final response in peer sections. A response arriving while the inspector is open updates the response section. Recordings made before complete-response retention explicitly report the missing payload rather than reconstructing it from separate tool and message events.

HTTP CONNECT uses its original authority-form target (`host:port`) and omits `scheme`. HTTP requests observed inside the tunnel retain their own HTTP/HTTPS scheme. Older CONNECT URLs are normalized for display without rewriting their raw evidence.

Chat groups messages and tool calls by their observed model request/response membership. Each response has a card labeled with the same chronological Model call number as the trajectory. Text and tools retain their first-observed order within that card, and tools are indented. A result carried by a later request updates its original tool card. Evidence without an observed model owner appears under Unassigned events until that ownership becomes available. Tool cards start collapsed with a parameter preview, capture status and observed elapsed time. Expanding a card loads complete parameters and returned content through the existing payload endpoint. The inspect icon selects the same tool in the trajectory and full detail panel. Tool elapsed time includes the interval between the observed call and model-visible return, including Agent scheduling.

Streaming updates refresh the affected task and preserve its message and disclosure nodes. Following pauses when the reader scrolls into history, and Latest resumes it. Stop immediately enters a pending cancellation state while the collector completes cancellation. A failed or rejected task restores its prompt when the input has no newer draft. New model or tool activity clears the active retry indicator. Earlier diagnostics remain expandable. Chat uses a completed message's preview directly when its UTF-8 length matches the recorded full length. A shortened preview or an older record without a length triggers a full payload read when available. If only partial text is available, Chat shows the capture status beside it.

Each task has a right-aligned question with a right-aligned You label, and one left-aligned assistant card. Headers, content and bottom copy/inspect toolbars use separators and distinct surfaces. Message timestamps appear at the right of the bottom toolbar. Running response cards stay together in the expanded execution area. A response containing the final answer appears above the execution footer, retaining any tools from that same response. Response copy actions copy the original message Markdown in that response. The execution footer shows current state, observed tool count and elapsed time, with a spinner while running and a green dot after success. Tool cards expand into complete parameters and returned content.

Execution starts expanded while running. Successful completion closes it once, keeping the final answer visible; subsequent updates preserve manual disclosure state. Final answers use the model's `final_answer` phase. For recordings without phase labels, only the last message of a successfully completed task is treated as its final answer. Failed and interrupted tasks keep their partial response and error visible and do not use the completed state.

Streaming text is appended to stable text nodes; completed messages render Markdown through locally bundled marked and DOMPurify. Only passive formatting and links survive sanitization. Copy uses original Markdown, preserving the captured content. All content-copy controls show a checkmark after success and restore their icon after two seconds or when the pointer leaves.

Stopping one task leaves the sandbox usable. Losing the owning WebSocket ends the sandbox under the existing session lifecycle. Chat reports the ended state and disables task controls. The draft stays editable on the current page. After confirmed container deletion, the entire session directory (including journals, semantic payloads and configuration staging) is destroyed. Monitor pages stop following when the recording disappears; no post-session replay is retained. A removed sandbox and its running task cannot be resumed.

Trajectory parent cards pulse when newly captured visible activity arrives beneath them, including collapsed processes, tools, model calls and tasks. Activity uses journal sequences across semantic and resource records. The pulse expires after 800 ms across redraws, and expanding an old branch preserves its activity age.

Owner and independent monitor pages project the same journal using separate cursors. A monitor cannot start tasks or own the container.

| Resource | Limit |
| --- | --- |
| Submitted prompt | 8 KiB UTF-8 |
| Decoded model HTTP body, model SSE event, semantic payload | 32 MiB each |
| Zstd decoder window | 64 MiB |
| Semantic payload budget | 512 MiB per run |
| Trajectory resource history in the browser | Latest 20,000 records, with an omission notice |

Payload limits produce an explicit unavailable status. Complete resource events remain in the session journal beyond the browser history window. Other model protocols and exact tool attribution for long-lived Agent internals are subsequent milestones.

## Verification

The deterministic browser integration uses the real external Codex with a local Responses server. It exercises multiple tool rounds, a resumed turn, request decoding, parameter and output details, process/file/HTTP association, independent monitors and container cleanup. It makes no remote model requests.

The AgentKit browser test runs the independent Go executable with a local HTTPS model fixture. It checks that text appears while the response is still open, two turns share history and have separate traces, Stop clears history, and a rejected credential reaches Chat. It scans sandbox state for the fixture credential. The test uses OpenSSL to issue its temporary certificates.

```bash
cargo test -p agenttomo-box --bin agenttomo-box
cargo test -p agenttomo-box --test http_capture
node --test crates/agenttomo-box/tests/agent-model.test.mjs
AGENTTOMO_PLAYWRIGHT_MODULE=/path/to/playwright-core/index.mjs \
  node crates/agenttomo-box/tests/agent-chat.browser.mjs
AGENTTOMO_PLAYWRIGHT_MODULE=/path/to/playwright-core/index.mjs \
  AGENTTOMO_AGENTKIT_BIN="$PWD/example/agent/agentkit/dist/agenttomo-agentkit" \
  node crates/agenttomo-box/tests/agentkit-chat.browser.mjs
```

The browser integration expects the tools image, Docker access and a Chromium CDP endpoint at `http://127.0.0.1:19222`, configurable through `AGENTTOMO_CDP`.
