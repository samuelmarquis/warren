# warren

A terminal dashboard for running many **Claude Code** agents at once over SSH.

A warren is a maze of interconnected burrows where a colony lives — and
*survives*. Each agent runs in its own burrow (an independent daemon process);
you tunnel between them through a left-hand sidebar; and everything outlives
the SSH connection it was started in.

```
┌────────────────┬───────────────────────────────────────────┐
│ > refactor-db  │  ● Claude Code                              │
│   web-ui     * │                                             │
│   infra-tf   ! │  > running tests…                           │
│   scratch      │  ✔ 42 passed                                │
│                │                                             │
│   + new agent  │  > _                                        │
└────────────────┴───────────────────────────────────────────┘
   ↑ sidebar: every agent           ↑ the focused agent's live TUI
```

- **Left column** — one row per agent. Bold = ready, dim = working, `!` =
  blocked on a permission prompt, `*` = finished while you were elsewhere.
  Optional per-agent tab colors. A pinned `+ new agent` tab at the bottom.
- **Right pane** — the focused agent's actual Claude Code TUI, full screen.
  Scrollback is Claude's own (fullscreen renderer); the wheel reaches it.
- **Survives disconnects** — drop your SSH session (cleanly *or* by yanking
  the cable) and every agent keeps running. Reconnect, run `warren`, and the
  identical view reassembles.

## How it's built (v1)

One Rust binary, two kinds of process:

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

- Each **agent daemon** owns the pty running Claude and an embedded terminal
  emulator. Viewers get a snapshot of styled cells on attach and a coalesced
  damage stream after that — no raw-escape replay, no resize mismatch.
- The **dashboard is stateless**: kill it, kill SSH under it, nothing is lost.
  All durable state (screen, name, color, hook state) lives in the daemons.
- **Nothing ever blocks on a peer.** Every connection has a bounded outbound
  queue; a stalled viewer is dropped (it reconnects to a fresh snapshot)
  instead of freezing the world — the failure mode that ended v0.
- **Agent states are pushed, not polled**: Claude Code lifecycle hooks run
  `warren hook <state>`, which pokes the agent's own daemon socket.

## Install

```sh
cd ~/Developer/warren && ./install.sh   # cargo build --release + install
```

Installs a single `warren` binary into `~/.local/bin` (override with
`PREFIX=…`). Requires a Rust toolchain. No ncurses, no Python.

## Use

```
warren                 open the dashboard (rebuilds the view from running agents)
warren new NAME [DIR] [COLOR 0-255] [new|resume|continue] [session-id]
           [--sys=SYSTEM-PROMPT] [--extra=EXTRA-CLAUDE-ARGS]
warren ls              list agents and their states
warren kill NAME       terminate an agent
warren attach NAME     view a single agent raw (no sidebar; Ctrl-\ detaches)
warren sessions        all resumable Claude sessions (id, mtime, cwd, title)
warren help
```

### Inside the dashboard

Modal, like vim. **CLAUDE** mode (default) sends every key verbatim to the
focused agent; **Ctrl-Space** toggles **NORMAL**; editing agent metadata or
the new-agent form is **EDIT** mode. The status bar's mode chip is color-coded
(CLAUDE orange, NORMAL green, EDIT purple):

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
| `x` | close agent (y/n confirm) |
| `:` | command line — `:q` detach · `:q!` kill all and quit · `:color #hex\|index` |

`Ctrl-\` detaches from anywhere. Mouse: click sidebar rows to focus, wheel
over the sidebar cycles, clicks/wheel over the pane go to the agent (Claude's
fullscreen TUI handles its own scrolling), palette swatches are clickable.

The **new-agent form** (the `+` tab): pick `new` / `resume` / `continue`,
title, root dir, a session from the resume picker (most-recent first, across
all projects), a system prompt (`--system-prompt`), extra claude CLI args,
and a tab color. NORMAL always navigates away — the form never traps you.

## Files

```
~/.warren/run/<name>.sock   one unix socket per live agent daemon
~/.warren/hooks.json        Claude Code hook settings (regenerated on spawn)
```

That's all v1 writes. (Other `~/.warren` entries are v0 leftovers.)

## Repository layout

- `v1/` — warren itself (Rust, single crate). Tests: `cargo test` — includes
  headless integration tests that spawn real daemons around scripted children,
  including a regression test for the v0 freeze (a viewer that stops reading
  must be dropped while the daemon keeps serving).
- `abduco/`, `dvtm/`, `bin/` — **retired v0** (patched C + shell). Kept until
  the v1 cutover has soaked; their patch sets live on `warren` branches of the
  two nested repos.
