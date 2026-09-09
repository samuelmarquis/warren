# warren

**A meta-harness for Claude Code: run a colony of agents from one terminal.**

Claude Code is a harness for one agent. warren is the layer above it — a
dashboard for running *many* Claude Code agents at once, each in its own
independent process, switched between like vim buffers, over an SSH
connection you're allowed to lose.

A warren is a maze of interconnected burrows where a colony lives — and
*survives*. Each agent runs in its own burrow (a daemon that answers to no
terminal); you tunnel between them through a left-hand sidebar; and
everything outlives the connection it was started in.

```
┌────────────────┬───────────────────────────────────────────┐
│ > refactor-db  │  ● Claude Code                            │
│   web-ui     * │                                           │
│   infra-tf   ! │  > running tests…                         │
│   scratch      │  ✔ 42 passed                              │
│                │                                           │
│   + new agent  │  > _                                      │
└────────────────┴───────────────────────────────────────────┘
   ↑ sidebar: every agent           ↑ the focused agent's live TUI
```

- **One row per agent**, state pushed by Claude Code's own lifecycle hooks —
  no polling. Bold = ready for you, plain = working, `!` = blocked on a
  permission prompt, `*` = finished while you were looking elsewhere,
  `z` = asleep.
- **Sleep the agents you aren't using.** `Z` stops a tab's claude process and
  gives its memory back; the tab stays, and waking it resumes the same
  conversation where it left off.
- **The right pane is the real thing** — the focused agent's actual Claude
  Code TUI, full screen, with working mouse, colors, and Claude's own
  scrollback.
- **Survives disconnects.** Drop SSH cleanly or yank the cable: every agent
  keeps running. Reconnect, type `warren`, and the identical view reassembles
  from the daemons. The dashboard holds no state worth losing.
- **Resume anything.** The new-agent form lists every resumable Claude
  session on the machine, most recent first, with titles — pick one and it
  reopens in a fresh burrow. Per-agent system prompts and extra CLI args too.
- **One static Rust binary.** No tmux, no screen, no ncurses, no Python, no
  config file.

## Why

Running a fleet of agents from a phone or laptop against a headless box
exposes every weakness of the classical multiplexer stack: replay artifacts
on reattach, scrollback fights with fullscreen TUIs, and — fatally — one
slow reader freezing every session at once. warren v0 was patched abduco +
forked dvtm + scripts; the seams between those layers generated a new
failure mode for every one they fixed.

v1 is a ground-up rewrite around one rule: **nothing ever blocks on a
peer.** Each agent is its own daemon owning a pty and an embedded terminal
emulator; viewers get a snapshot of styled cells plus a damage stream, never
a raw escape-sequence replay. Every connection has a bounded outbound queue
— a stalled viewer is dropped (and reconnects to a clean snapshot) rather
than wedging an agent. There's a regression test that stalls a viewer
mid-firehose and asserts the daemon doesn't care.

```
your terminal ──> warren            (dashboard: a STATELESS viewer)
                    │ one unix socket per agent (~/.warren/run/<name>.sock)
        ┌───────────┼───────────┐
        ▼           ▼           ▼
   warren __daemon …           …    (one per agent, ppid 1, independent)
   pty + embedded terminal (alacritty_terminal) + name/color/state
        │
      claude
```

Kill the dashboard, kill SSH under it, kill nine of ten daemons — whatever
is left keeps working. Agent state (screen, name, color, hook state) lives
in the daemon; the daemon *is* the agent.

## Install

Requires a Rust toolchain and [Claude Code](https://claude.com/claude-code).
Developed and daily-driven on macOS (a headless Apple Silicon machine over
SSH); the code is plain POSIX + rustix and should work on Linux, but it
hasn't soaked there yet.

```sh
git clone https://github.com/samuelmarquis/warren
cd warren && ./install.sh        # cargo build --release → ~/.local/bin/warren
```

Override the destination with `PREFIX=…`.

## Use

```
warren                 open the dashboard (rebuilds the view from running agents)
warren new NAME [DIR] [COLOR 0-255] [new|resume|continue] [session-id]
           [--sys=SYSTEM-PROMPT] [--extra=EXTRA-CLAUDE-ARGS]
warren ls              list agents and their states
warren kill NAME       terminate an agent
warren sleep NAME      stop its claude process, keep the agent (resumable)
warren wake NAME       start it again on the same conversation
warren attach NAME     view a single agent raw (no sidebar; Ctrl-\ detaches)
warren sessions        all resumable Claude sessions (id, mtime, cwd, title)
warren help
```

### Inside the dashboard

Modal, like vim. **CLAUDE** mode (default) sends every key verbatim to the
focused agent; **Ctrl-Space** toggles **NORMAL**; editing agent metadata or
the new-agent form is **EDIT** mode. The status bar's mode chip is
color-coded (CLAUDE orange, NORMAL green, EDIT purple):

| NORMAL key | action |
|---|---|
| `j`/`k`, arrows | focus next / previous |
| `1`-`9`, `0` | jump to row N (an empty row opens the new-agent form) |
| `Shift+digit` | swap the focused agent with row N |
| `g` / `G` | first / last agent |
| `n` | new-agent form |
| `i` `a` `l` `Enter` `Esc` | back to CLAUDE |
| `r` | rename (pins the name against Claude's title sync) |
| `e` / `c` | edit form: title + 256-color picker |
| `Z` | sleep the agent (or wake a sleeping one) |
| `x` | close agent (y/n confirm) |
| `:` | command line — `:q` detach · `:q!` kill all · `:color #hex\|index` |

`Ctrl-\` detaches from anywhere. Mouse: click sidebar rows to focus, wheel
over the sidebar cycles agents, clicks and wheel over the pane go to the
agent (Claude's fullscreen TUI handles its own scrolling), palette swatches
are clickable.

The **new-agent form** (the `+` tab): pick `new` / `resume` / `continue`,
title, root dir, a session from the resume picker, a system prompt
(`--system-prompt`), extra claude CLI args, and a tab color. Tab/Shift+Tab
cycle fields; NORMAL always navigates away — the form never traps you.

### Sleep mode

A colony costs what its members cost, and an idle Claude Code still holds
its several hundred megabytes. **Ctrl-Space `Z`** stops the focused agent's
claude process — and its whole process group, so tool children and MCP
servers go too — while keeping the agent itself: the daemon lives on, so the
tab keeps its row, name, color, working directory and the last screen claude
painted, dimmed under a sleeping badge. `Z` again (or just typing at it)
respawns `claude --resume <session-id>` in the same burrow, and the
conversation carries on. Keys typed at a sleeping agent are buffered and
land in the resumed prompt.

The session id comes from the agent's own lifecycle hooks: Claude passes one
on stdin with every hook event, so warren knows it a second after spawn and
keeps it current. Resuming appends to the same session, so an agent can sleep
and wake forever without ever forking its conversation.

Two refusals, both about not losing anything you can't get back. Sleeping
**mid-turn** is refused (`AGENT BUSY` in the status bar) — the in-flight turn
would be lost, and the transcript would end on a tool call that never
returned; let it finish, or interrupt it yourself first. Sleeping before the
first hook has reported a session id is refused too (`NO SESSION YET`), since
there would be nothing to resume from.

The stop itself is SIGTERM to the process group, which Claude Code exits on
cleanly after running its `SessionEnd` hooks — SIGKILL follows only if it
ignores that. A wake whose `--resume` fails leaves the agent asleep with
claude's error frozen on screen, rather than taking the tab down with it.

Sleep is *not* persistence: agents still die with the machine. It buys memory
back within a session, and the conversations were always resumable anyway.

### How agent states work

warren generates a Claude Code settings file whose lifecycle hooks run
`warren hook <state>`, which pokes the agent's own daemon over its socket:
prompt submitted or tool running → *working*, turn finished → *ready*,
permission prompt → *attention*. The hook also reads the `session_id` out of
the JSON payload Claude hands it on stdin — that's what sleep resumes from.
Outside warren the hook is a silent no-op, it never blocks on stdin or on the
socket, and it always exits 0 — a wedged daemon can never stall Claude.

## Files

```
~/.warren/run/<name>.sock   one unix socket per live agent daemon
~/.warren/hooks.json        Claude Code hook settings (regenerated on spawn)
```

That's everything warren writes. Agents die with the machine (conversations
persist in `~/.claude` and come back through the resume picker).

## Development

```sh
cargo test
```

Unit tests plus headless integration tests that spawn real daemons around
scripted children and drive them over the socket — snapshot fidelity, damage
streaming, resize fan-out, hook round-trips, exit reaping, sleep/wake (the
process really dies, the agent really doesn't), and the stalled-viewer
regression test.

```sh
cargo test sheep -- --nocapture     # watch the flock
```
