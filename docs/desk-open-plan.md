# Open documents from the agent pane

Status: Implemented. This is a completed design record, not an active plan.
Agent opening and explicit desk links are implemented. See the
[plan index](README.md) and [remaining gaps](remaining-gaps-plan.md).
The opt-in terminal adapter is documented in [desk-links.md](desk-links.md).

## Goal

Inside `margin desk -- claude`, asking Claude to find a document and show it
in Margin switches the existing Margin viewer to that document while keyboard
focus stays in Claude. Deliver agent-invoked opening first, then clickable links.

Today, the desk arranges independent tmux panes. `margin content/article.md`
starts a viewer; it does not control the existing desk viewer. The existing
viewer already refreshes displayed documents after external edits.

## 1. Add the command

```sh
margin open content/article.md
margin open content/article.md --line 42
margin open content/article.md --json
```

The command sends a request to the running desk's Margin viewer. Paths are
repository-relative, consistent with existing Margin commands. A line number
opens source view at that line; otherwise, the document opens in reading view.

If no viewer is available, return a clear error with instructions. Success means
the viewer accepted and displayed the document, not merely that a request was
sent. Use the existing CLI output and error conventions.

Implementation entry point: `src/cli.rs`.

## 2. Connect each desk's panes

Give each desk a private local Unix socket. Pass its address to both processes
through a `MARGIN_DESK_SOCKET` environment variable. Separate desks, including
desks in the same repository, must target their own viewers.

Margin owns the socket's lifetime. Handle startup races, disconnection, bounded
timeouts, and cleanup. Keep protocol messages bounded and put the socket in a
directory accessible only to the current user. Ensure detaching the desk does
not remove a socket still used by its viewer.

Preserve direct execution of agent arguments. Do not modify global tmux
configuration or use tmux keystrokes as a program API.

Implementation entry point: `src/integrations/tmux.rs`, with a dedicated local
transport module shared by the command and viewer.

## 3. Handle requests in the TUI

Integrate requests into `src/tui/run.rs` and reuse document navigation in
`src/tui/shell.rs`.

- Validate paths with existing scope, file-type, containment, and symlink rules.
- Switch documents without restarting the viewer.
- Keep keyboard focus in the agent pane.
- Return a clear viewer-busy response during an external editor handoff or
  unfinished confirmation/input. The transport must remain responsive even
  while the terminal event loop is suspended for an editor.
- Leave the current document untouched on failure.
- Acknowledge successful opening after the viewer applies the request and draws
  the document.

Opening a document must not checkpoint, approve changes, or alter review
decisions. The feature initially supports documents eligible for Margin's
existing viewer; it does not expand Margin into an arbitrary-file browser.

## 4. Document agent usage

Provide a short instruction users can add to agent guidance:

> When asked to show a document in Margin, find its repository-relative path and
> run `margin open PATH`. Use `--line N` for a particular passage.

The command works with any agent capable of running shell commands.
Claude-specific hooks are not required. Do not silently modify user agent
configuration.

## 5. Verify the workflow

Extend existing CLI, TUI, and tmux tests to verify:

- An agent-side command changes the existing viewer rather than spawning one.
- Line navigation, spaces, and Unicode paths work.
- Multiple desks remain isolated.
- Missing, unsupported, and out-of-scope files fail without changing the viewer.
- Busy, unresponsive, or closed viewers return bounded, useful errors.
- Startup races and malformed or oversized messages are handled safely.
- Detaching and reattaching preserves the connection.
- Narrow layouts update Margin's separate window without stealing focus.
- Opening does not change content, checkpoint state, or review decisions.

Update `README.md`, the desk contract in `SPEC.md`, and the relevant architecture
decision to describe the new communication channel. Preserve existing terminal
lifecycle and desk isolation guarantees.

## 6. Follow-up: clickable links

Verify how supported terminals and Claude output expose clickable file links
before choosing an integration. Then provide an adapter that routes clicks
through the same `margin open` mechanism.

A link must identify the intended desk as well as the file. Do not assume
arbitrary printed paths automatically route to Margin. Terminal-specific setup
and the behavior when a linked desk has closed must be documented and tested.

## First milestone acceptance

Inside `margin desk -- claude`, the user asks: "Find the document about X and
show it in Margin." Claude locates the document and invokes `margin open`.
The existing left pane displays it, the command reports success, and the user
can continue typing in Claude without switching focus.
