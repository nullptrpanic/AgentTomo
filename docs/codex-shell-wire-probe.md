# Codex shell tool traffic probe

## Result

On 2026-11-09, the externally installed **Codex CLI 0.167.0**, using
**gpt-6-astra**, successfully executed shell commands inside the existing
AgentTomo Docker environment. Actual model traffic used a **Code Mode custom
tool**, rather than a top-level JSON function call named `bash` or
`exec_command`.

The observed structure was:

```text
Model response: custom_tool_call, name = exec, call_id = call_...
  input: JavaScript calling tools.exec_command({cmd: ...})
    local execution: /bin/bash -c <command>
```

Matching an explicit tool name remains useful, but the name must be interpreted
at the correct layer. `exec` identifies the JavaScript orchestration tool; it
does not establish that its contents are shell commands. This is a result for
the tested version, model, and configuration, not a universal Codex contract.

## Test environment and source of evidence

- Reused the host-installed Codex distribution through a read-only Docker mount,
  including its `codex-code-mode-host` companion executable.
- Used the existing `agenttomo-playground:24.04` image and AgentTomo's built-in
  proxy and process collector. The test command ran as UID 1000 with Codex's
  read-only execution sandbox.
- Copied the external Codex configuration and ChatGPT login into a private,
  disposable `CODEX_HOME`. The test retained the configured model and reasoning
  effort; hooks were disabled and sessions were ephemeral.
- Added a temporary provider pointing to a loopback forwarding endpoint and
  disabled WebSocket transport for this SSE probe. The forwarder used the
  machine's existing HTTP proxy to contact the real ChatGPT Codex backend.
  These overrides were local to the test; the original configuration was not
  modified.
- Examined real HTTP request/response bodies, including the model's SSE stream.
  Codex rollout/session JSONL files were not used. Process execution evidence
  came from AgentTomo's own ptrace recording.
- Disabled file-content previews. Temporary credential copies and containers
  were removed after the tests; recorded authentication headers were redacted.

The forwarding endpoint was needed because direct access to `chatgpt.com`
timed out in this environment. This probe validates the actual model payload
through AgentTomo's HTTP collection path; it does not establish transparent
Codex TLS interception or WebSocket rewriting support.

## Actual request and response shapes

The outbound request used `Content-Encoding: zstd`. Decoding the captured body
showed `stream: true`; tool definitions were carried in an `additional_tools`
input item. That item declared namespace `functions`, with a custom tool named
`exec` whose description specifies JavaScript orchestration. Its format was a
Lark grammar accepting JavaScript source. The request did not have a top-level
`tools` field in this capture.

The shell probe's completed tool item was equivalent to this excerpt, with
identifiers shortened and unrelated metadata omitted:

```json
{
  "type": "custom_tool_call",
  "id": "ctc_...",
  "call_id": "call_...",
  "name": "exec",
  "input": "const result = await tools.exec_command({cmd: \"printf 'AGENTTOMO_CODEX_SHELL_PROBE\\\\n'; pwd\", max_output_tokens: 1000});\ntext(result.output);\n",
  "status": "completed"
}
```

The stream contained these events for the call:

1. `response.output_item.added`
2. **40** `response.custom_tool_call_input.delta` events
3. `response.custom_tool_call_input.done`
4. `response.output_item.done`

The stream also contained `response.completed`. This call did not use
`response.function_call_arguments.delta`. A subsequent real model request
included `custom_tool_call_output` with the same outer `call_id` and the shell
output. The final answer was:

```text
AGENTTOMO_CODEX_SHELL_PROBE
/probe/workspace
```

AgentTomo independently observed the final `/bin/bash -c` execution as PID 108,
process instance 37. The shell snapshot setup and final shell execution used
the same process instance. These PIDs are evidence from this run only.

The response body used SSE framing, but its HTTP `Content-Type` header was
absent in this route. AgentTomo therefore retained it as a normal HTTP body,
not in its SSE message store. A future model-protocol adapter needs a bounded
classification path for supported endpoints; a strict Content-Type-only check
does not cover this observed response.

## Existing execution environment identifiers

A second successful probe printed only `CODEX_TOOL_CALL_ID`,
`CODEX_THREAD_ID`, and a test marker. The values showed:

```text
Model tool call ID:      call_2339469a31824018bb724b612d02432c
Shell CODEX_TOOL_CALL_ID: exec-eb2bc2da-bc92-4804-b6d3-7cbb50278784
```

These are different IDs. The environment variable can identify the nested
execution, but it must not be treated as the outer model `call_id` without an
observed mapping. The first shell probe's ordinary result did not expose that
mapping. Printing the environment variable in this diagnostic command is not
a general collection strategy.

## Follow-up: an explicit mapping through OpenTelemetry

Two additional live probes on the same Codex CLI **0.167.0** established the
missing mapping using Codex's existing OTLP/HTTP **trace exporter**. A temporary
loopback receiver collected JSON exports alongside the unchanged model traffic.
Hooks remained disabled; no app-server integration or Codex session-file reader
was used. The original Codex configuration was not changed.

The observed records were:

```text
Span: code_mode.handler.execute
  conversation.id = <Codex conversation>
  call_id         = call_<outer model call>
  cell.id         = 1

Span event: codex.code_mode.nested_tool_dispatched
  conversation.id      = <same conversation>
  cell.id              = 1
  runtime_tool_call_id  = tool-1
  call_id              = exec-<local execution>
  tool_name            = exec_command

Command environment:
  CODEX_THREAD_ID    = <same conversation>
  CODEX_TOOL_CALL_ID = exec-<same local execution>
```

The model `call_id` in the first span exactly matched the actual SSE tool item.
The nested execution ID exactly matched the identifier printed from the command
environment. Joining the observed cell within its runtime/conversation scope
therefore provides an explicit parent relationship; matching command text or
timestamps is not needed. The adapter must scope cell IDs to a runtime instance
and handle restarts/reuse rather than treating `cell.id = 1` as globally unique.

- `run-20261109T141457`: one outer call, one nested command, shell PID 103.
- `run-20261109T141701`: one outer call, two concurrent nested commands with
  identical command strings, shell PIDs 111 and 112. The two command spans
  overlapped in time and had distinct execution IDs (`tool-1` / `tool-2` within
  the same cell). Both IDs matched the environment identifiers in the outputs.

Both runs exited with code 0 and reached collection state `complete`. Their
`correlation-verification.json` files record the assertions. PID observations
come from AgentTomo's process collector; environment values in these probes were
printed by the diagnostic commands. Production ptrace environment capture and
automatic per-process binding are still to be implemented.

Logs alone are insufficient for the demonstrated cell mapping: the mapping was
present in exported spans and span events. `codex.tool_result` logs provided
additional arguments/results. The earlier `executed_tool_call_metadata` feature
probe (`run-20261109T133952`) supplied nested names and arguments in the next model
request, but did not supply execution IDs; that feature is not required for the
OTel approach.

### Network context and coverage

Each probe's two model HTTP requests carried W3C `traceparent` headers. The
header's `(trace_id, span_id)` exactly matched an exported
`endpoint_session.stream_encoded_json_with` span. This verifies a concrete
request-to-span association for **model traffic**. It does not establish that
every internal tool's HTTP client propagates trace context. Only requests with
verified context and a trace path to the relevant tool may receive that tool's
identity; sharing a PID or a trace ID alone is insufficient.

Codex's native web search is a hosted tool, separate from sandboxed local command
networking. AgentTomo can associate observed search invocation/results, but
cannot treat an unobserved provider-side website fetch as locally captured
traffic. See the [official web-search documentation](https://learn.chatgpt.com/docs/web-search).

### Minimal integration proposed by the probes

1. Let the launcher enable a run-scoped local OTLP/HTTP JSON endpoint through
   temporary Codex configuration. Use the existing model proxy for actual model
   requests/results; use telemetry for execution relationships. Codex documents
   both log and trace exporters in the [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).
2. Keep Codex span/event names and `CODEX_*` identifiers inside one adapter.
   Normalize external identifiers into AgentTomo tool calls, retaining their
   evidence and scope. Give outer model calls and nested executions separate
   `agenttomo_toolcall_id` values with an explicit parent relationship.
3. At a successful ptrace exec stop, read only the allowlisted correlation
   environment variables. Bind the execution to the existing
   `process_instance_id`, preserving PID reuse handling and child inheritance.
   A new execution marker must replace an inherited binding when appropriate.
4. Reuse existing file/process/network observations. Scope network attribution
   to the available evidence: the current TCP registry records the connect
   initiator, not necessarily the writer of every request on a shared socket.
   HTTP requests with propagated trace context may be associated at request
   granularity instead.
5. Accept telemetry arriving after resource events and append relationship
   updates without blocking forwarding or rewriting existing evidence. Missing
   parent/context data remains unresolved; do not fall back to an exact claim
   based on timing.

The minimum next implementation milestone is the shell execution chain, including
two identical concurrent commands and their independently observed resource
operations. Internal HTTP/MCP client coverage needs separate path-specific tests.
No production adapter, collector, SSE rewriting, or environment binding was added
by these probes.

These trace names/attributes are version-specific observations, not a guarantee
for every Codex release. The two small probes exported approximately 3.5 MB and
4.0 MB of JSON telemetry, including unrelated internal spans. Production should
retain only required relationships and bounded context, but receiver-side
filtering does not remove Codex's export overhead. Export batching and span
completion also mean attribution can arrive after execution.

## Implications for the next implementation milestone

- The OTel bridge above provides a lower-interference first path for this Codex
  version. The following rewriting observations remain relevant if an optional
  command-marking path is considered later; rewriting is not needed for the
  verified Codex execution-ID relationship.
- Keep explicit tool-name mappings; do not classify arbitrary names by whether
  they contain `exec`, `shell`, or `bash`.
- For a supported direct shell function, validate the expected argument shape
  and command field before applying an environment-prefix rule.
- For the observed `custom_tool_call` named `exec`, first classify its input as
  JavaScript. Do not prepend shell syntax to the custom tool input. Supporting
  `tools.exec_command` inside this input requires a separate, bounded JavaScript
  adaptation strategy; a string match is insufficient for dynamic arguments,
  aliases, comments, or multiple nested calls.
- Preserve the distinction between an outer model tool call and its nested
  executions. One `exec` input can invoke multiple tools.
- Treat `webfetch`, unknown tool shapes, and unsupported custom input as
  observation-only. Do not change their arguments.
- Keep original and modified payload provenance separate when rewriting is
  introduced. No production SSE rewriting or environment-to-process binding
  was added by this probe.

## Retained evidence and verification

The sanitized [SSE fixture](../crates/agenttomo-box/tests/fixtures/codex-code-mode-shell.sse)
retains the actual 40 input-delta boundaries and the corresponding added/done
events. Identifiers are normalized; unrelated response events and internal
metadata are omitted, so this is a tool-event fixture rather than a complete
replayable API response.

Local raw evidence is under the ignored `.agenttomo/codex-wire-probe/` directory:

- `run-20261109T131618`: successful shell execution and real model output.
- `run-20261109T131815`: successful execution-identifier probe.

Both runs exited with code 0 and reached AgentTomo collection state `complete`.
Verification checks the captured response against AgentTomo's retained HTTP
body, delta reconstruction against completed tool input, the next request's
tool-result ID, actual shell execution, and the expected stdout. An earlier
attempt failed because only the main Codex executable was mounted; mounting
the complete external distribution fixed the missing Code Mode host. Codex
also reported that it used its bundled bubblewrap because no system
`bubblewrap` was installed in the image.
