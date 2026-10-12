# Claude hook payload fixtures

The captured payloads of the upstream Claude Code hook contract the
adapter pins (SPEC 12.2, task T048). Each file holds one event exactly as
the upstream reference documents its input: the common fields every event
carries, plus the fields of that event. The adapter validates
`hook_event_name`, `cwd`, and `stop_hook_active`, and ignores every other
field, so a newer upstream schema cannot fail an installed adapter.

The identifier values are well-formed examples, not identifiers of a real
session. The string `@@WORKSPACE@@` stands for the working directory of
the test repository; `tests/integrations.rs` substitutes it before it
pipes the payload to the built binary.

| Fixture | Event | What the case proves |
| --- | --- | --- |
| `user-prompt-submit.json` | `UserPromptSubmit` | Outstanding content is captured before a prompt; a refused capture blocks the prompt with the supported response. |
| `stop.json` | `Stop` | A completed pass is captured while the response stays minimal, so the agent stops. |
| `stop-continued.json` | `Stop` with `stop_hook_active: true` | A Stop that arrives after a continuation still never blocks, so Margin cannot form a continuation loop. |
| `stop-failure.json` | `StopFailure` | The boundary is best effort: the capture happens, the failure text is ignored, and no output reaches the agent. |
| `session-start.json` | `SessionStart` | A best-effort boundary capture at startup; plain stdout on this event is agent context, so the adapter prints none. |
| `session-end.json` | `SessionEnd` | A best-effort boundary capture at the end; output is discarded, so a capture failure is one stderr diagnostic only. |

## Provenance

The field names and shapes come from the Claude Code hooks reference
(https://code.claude.com/docs/en/hooks, retrieved 2026-10-11). The
prompt, message, and title strings are written for these fixtures; they
are not captured user content.
