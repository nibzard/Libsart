# Explicit desk links

The primary agent workflow is `margin open PATH [--line N]`: it needs no
terminal setup. A printed filename or an agent's ordinary `file://` link
has no Margin desk identity and does not control the viewer.

## Opt-in kitty adapter

Kitty's documented [open actions](https://sw.kovidgoyal.net/kitty/open_actions/)
can match a URL protocol and launch a command in the background. Add this
entry to your own `~/.config/kitty/open-actions.conf`, using the absolute
path of the Margin executable you want to run:

```text
protocol margin
action launch --type=background /absolute/path/to/margin open --url ${URL}
```

Margin does not install or modify terminal or agent configuration. This
adapter runs an explicit argv; it does not evaluate the URL as shell text.
The background command uses the desk identified by the link, so another
active desk or a different current directory cannot redirect the request.

From a shell in the desk's agent pane, emit a link:

```sh
margin open 'content/My Draft.md' --line 42 --link
```

Ctrl+Shift+click the link using kitty's default link binding. The background
command opens source view in that desk's existing viewer. The action opens
no terminal window and selects no tmux pane, so focus stays where it was.
Use `margin open PATH --link --json` to obtain the URL for a renderer that
explicitly supports custom hyperlink targets. Link generation does not open
the document or claim that the viewer displayed it.

When tmux is in the display path, OSC 8 hyperlinks require tmux 3.4 or newer
and an outer terminal advertising hyperlink support; see the upstream
[tmux 3.4 changes](https://github.com/tmux/tmux/blob/3.4/CHANGES).
This optional link feature has a higher tmux requirement than agent-invoked
opening. The launcher changes no global tmux settings. If your terminal
chain suppresses hyperlinks, use `margin open PATH` directly.

## Agent output and other terminals

Claude Code's file links use file URLs in versions with OSC 8 file-link
support; the upstream [file-link issue](https://github.com/anthropics/claude-code/issues/18717)
also records differences between terminal renderers. These file URLs do not
carry Margin's socket identity. Tool-output escape sequences can be captured
or reformatted by an agent, so asking Claude to print a filename or echo an
OSC 8 sequence is not a supported automatic click integration. Ask the agent
to run `margin open PATH`, or use an explicit link in a renderer that preserves
its `margin://` target. No Claude hook is needed.

[iTerm2's OSC 8 documentation](https://iterm2.com/documentation-escape-codes.html)
likewise routes URLs to their configured handlers. Its default semantic
history and file URL handling do not dispatch to Margin. No iTerm2 or system
URL handler is installed by this feature. The supplied adapter is for kitty;
other terminals require their own explicit handler and validation.

## Closed desks and verification

A link names a unique, local socket directory. Detaching and reattaching
keeps that identity. Quitting the viewer removes the socket; following its
old link returns a bounded unavailable error and never opens another desk.
A background kitty action may hide stderr. To inspect an error, copy the URL
and run `margin open --url 'margin://open/...' --json` in a shell; then start
a new desk and generate a new link if the old viewer has closed. Links are
local to the machine running the desk and are not cross-host links.

Automated tests cover URL round trips (spaces, Unicode, shell punctuation),
malformed URLs, the CLI adapter, isolation from a different inherited desk,
and closed desks. tmux tests cover focus and detach/reattach for both layouts.
A physical kitty click and agent-rendered custom links require a manual
terminal check; their appearance is not inferred from CLI success.
