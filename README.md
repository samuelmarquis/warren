# warren

**A meta-harness for Claude Code and OMP: run a colony of agents from one
terminal.**

Claude Code is a harness for one agent; so is OMP. warren is the layer above
them — a dashboard for running *many* agents at once, each in its own
independent process, switched between like vim buffers, over an SSH
connection you're allowed to lose. Pick the harness per agent when you create
it; nothing after that treats them differently.

A warren is a maze of interconnected burrows where a colony lives — and
*survives*. Each agent runs in its own burrow (a daemon that answers to no
terminal); you tunnel between them through a left-hand sidebar; and
everything outlives the connection it was started in.

```
┌────────────────┬───────────────────────────────────────────┐
│ 1 warren/      │  ● Claude Code                            │
│ ├ 1 refactor-db│                                           │
│ └ 2 web-ui   * │  > running tests…                         │
│ 2 infra/    ▸3 │  ✔ 42 passed                              │
│                │                                           │
│   + new agent  │  > _                                      │
└────────────────┴───────────────────────────────────────────┘
   ↑ agents, by directory           ↑ the focused agent's live TUI
```

- **Grouped by what's inside home.** One folder per thing directly under
  `~` — every checkout in `~/Developer` is *one* row, not one row each;
  `^Space 1 2` is folder one, agent two, and clicking a folder folds it
  away.
- **One row per agent**, state pushed by Claude Code's own lifecycle hooks —
  no polling. Bold = ready for you, plain = working, `!` = blocked on a
  permission prompt, `*` = finished while you were looking elsewhere,
  `z` = asleep.
- **Sleep the agents you aren't using.** `z` stops a tab's claude process and
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
- **Agents on other machines** in the same sidebar as the ones here, grouped
  under a heading per machine, over one ssh connection each — and the
  new-agent form starts them there too, so nothing needs opening over there
  first.
- **One static Rust binary.** No tmux, no screen, no ncurses, no Python. The
  only file you would ever hand-edit is a list of machines.

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
           [--kind=claude|omp] [--sys=SYSTEM-PROMPT] [--extra=EXTRA-AGENT-ARGS]
warren ls              list agents and their states
warren kill NAME       terminate an agent
warren sleep NAME      stop its claude process, keep the agent (resumable)
warren wake NAME       start it again on the same conversation
warren attach NAME     view a single agent raw (no sidebar; Ctrl-\ detaches)
warren sessions [KIND] all resumable sessions (id, mtime, cwd, title) — claude, or omp
warren help
```

### Inside the dashboard

Modal, like vim. **CLAUDE** mode (default) sends every key verbatim to the
focused agent; **Ctrl-Space** toggles **NORMAL**; editing agent metadata or
the new-agent form is **EDIT** mode. The status bar's mode chip is
color-coded (CLAUDE orange, NORMAL green, EDIT purple):

| NORMAL key | action |
|---|---|
| `j`/`k`, arrows | focus next / previous (skipping folded folders) |
| digit, digit | jump: folder, then the agent inside it (`^Space 1 2`) |
| `Shift+digit` | move the focused agent to position N *of its folder* |
| `g` / `G` | first / last agent |
| `n` | new-agent form |
| `i` `a` `l` `Enter` `Esc` | back to CLAUDE |
| `r` | rename (pins the name against Claude's title sync) |
| `e` / `c` | edit form: title + 256-color picker |
| `z` | sleep the agent (or wake a sleeping one) |
| `x` | close agent (y/n confirm) |
| `:` | command line — `:q` detach · `:q!` kill all · `:color #hex\|index` |

`Ctrl-\` detaches from anywhere, and `Ctrl-Z` sleeps the focused agent from
anywhere — it is never forwarded, because a terminal's own `^Z` is SUSP and
a harness stopped by its tty is one warren cannot wake. Mouse: click sidebar rows to focus, click a
folder header to fold that directory away, wheel over the sidebar cycles
agents, clicks and wheel over the pane go to the agent (Claude's fullscreen
TUI handles its own scrolling; a harness that doesn't ask for the mouse gets
warren's scrollback instead — see below). On a form, everything is clickable — a chip
picks its option, a field row takes the cursor, a session in the resume
picker selects it, the wheel scrolls that list, and palette swatches have
always been clickable.

The **new-agent form** (the `+` tab): pick the machine (once there is more
than one), the harness (`claude` / `omp`), then `new` / `resume` /
`continue`, title, root dir, a session from that harness's resume picker, a
system prompt (`--system-prompt`), extra CLI args, and a tab color.
Tab/Shift+Tab cycle fields; NORMAL always navigates away — the form never
traps you.

### Two harnesses, one colony

`--kind=claude` (the default) and `--kind=omp` are the only place the choice
appears. Both harnesses spell `--continue`, `--resume` and `--system-prompt`
identically, so modes, the resume picker, per-agent system prompts, extra
args, title sync, folders, sleep and wake all work the same either way — and
the sidebar deliberately doesn't say which is which, because the row is
narrow and the pane already tells you.

What differs is how much an agent can say about itself. Claude Code runs
warren's generated lifecycle hooks, which push exact states and the session
id. OMP has no equivalent settings file, so its agents fall back to the
output-activity heuristic every viewer already computes: they read *working*
and *idle* correctly, but never `!`, because nothing tells us a permission
prompt is up.

Sleep still works, because OMP says which session it is running another way:
it writes `<cwd>` and the session file to
`~/.omp/agent/terminal-sessions/<tty>` at startup, and the daemon owns that
pty — so the row keyed by its own slave tty is that agent's own session, even
with several agents in one directory. A row is rejected if it predates the
agent (tty names get recycled) or names another directory.

Everything harness-specific lives in `src/kind.rs`: the command line for a
run, whether lifecycle state can reach the daemon, where sessions are kept,
and how a live agent's session is identified. A third harness is a variant
and those four answers.

### Folders

A folder is the thing *directly inside home* that an agent's working
directory is under, labelled by its own name and never by the path that got
you there. Agents in `~/Developer/Phylogen`, `~/Developer/warren` and
`~/Developer/plugins/dsp/src` all sit under one `Developer/`, because that
is how the work is actually filed; `~` itself is a folder, and a directory
somewhere else on the machine (`/opt/pmk/env`) is still just itself. Two
folders that would read the same take one parent component (`phylo/src/`,
`warren/src/`) and no more — unless they are the same place on different
machines, since `~/Developer` is `~/Developer` whoever's home it is and the
heading above the row already says which machine you are looking at.

Home is per machine: this one's is known, and another's comes over on its
roster line. A machine running a warren too old to report one has it guessed
from its own paths (`/Users/…`, `/home/…`), which is right often enough that
nothing looks wrong until it is updated.

Navigation is two digits: `^Space 1 2` is folder one, agent two. The first
digit arms a folder — the status bar names it and how many agents it holds —
and the second lands; Esc abandons the jump, and any other key is handled as
the NORMAL key it is, so a mistyped folder never eats the keystroke after it.
`Shift+digit` moves the focused agent within *its own* folder: folders come
from working directories, so renumbering can never smuggle an agent into a
directory it isn't in.

Click a folder header to fold it away. Folded agents keep running, the header
says how many are in there, and jumping into a folded folder opens it. The
fold is the one thing the dashboard remembers, it is per-viewer, and it is
deliberately forgotten when you detach — everything else still lives in the
daemons.

### Other machines

Put ssh destinations in `~/.warren/hosts`, one per line, and their agents
join this sidebar:

```
# machines
smq
servo@mini.local  /opt/homebrew/bin/warren   # where warren lives over there
```

The second column is optional and usually necessary: an ssh command runs
without your login shell's PATH, so `~/.local/bin/warren` is not on it.
Agents from this machine come first under its own hostname, then each host in
the order listed:

```
 solidgoldmagikarp
 1 Research/
 ├ 1 SVM-Encrypt
 2 warren/
 └ 1 Warren tab
 smq · reconnecting…
 3 Games/
 └ 1 Spork
```

Folder numbers run straight through, so `^Space 3 1` reaches an agent on
another machine with the same two digits as one here — the machine is a
heading, not another digit to type. Nothing else marks a row as remote: the
sidebar is 23 columns wide and the pane already tells you what you are
looking at.

Under it, per machine, is one `ssh … warren __roster` (which reports the
agent list and reprints it whenever it changes) and one `ssh … warren
__pipe <agent>` per agent, all sharing a single ssh connection through
ControlMaster. Each pipe carries that agent's socket on its stdio, and the
dashboard holds the near end of a socketpair — an ordinary unix socket, which
is what a viewer always talked to. The daemon protocol, the daemons, and the
poll loop are unchanged; the far machine only needs a warren new enough to
have `__pipe`.

BatchMode is forced on, because ssh's stdin is an agent's protocol stream and
a password prompt would read frames as a passphrase. Use keys or an agent; a
host that cannot connect says so on its row instead of hanging.

A machine that stops answering keeps its rows, dimmed and unfocusable, drawn
from the last thing warren saw there — so a wifi blip does not renumber the
sidebar out from under you mid-keystroke. warren keeps redialling (backing
off to every ten seconds), clicking its heading retries immediately, and when
it answers the rows go live again on their own. Nothing about a remote agent
is stored on this machine: sleep, wake, rename, colour and close all work
exactly as they do locally, because they are the same messages to the same
daemon.

Agents are made over there from the same form, which is why the machine
comes first on it: everything under that field means whatever it means on
the machine you picked. `warren new` runs on the far side, so that machine
names the agent and gives it a slot against its own agents, its own harness
starts it, and the row arrives here in its next roster like any other — the
resume picker fills from its sessions, and a root dir left at its default
travels as `~` rather than as a path only this machine has. Nothing has to
be running over there first: agent daemons answer to no terminal, and a
dashboard on that machine would only be a second viewer of the same ones.

`warren ls`, `kill`, `sleep` and `wake` on the command line are still
local-only; the dashboard is what spans machines.

### Scrolling back

The wheel over the pane belongs to whoever asked for it. Claude Code takes
the alternate screen and subscribes to the mouse, so the wheel goes straight
through and Claude scrolls its own history, exactly as before. A harness that
does neither — OMP streams to the primary screen and never enables mouse
tracking outside its fullscreen overlays — leaves the wheel to warren, which
scrolls that viewer through the agent's scrollback instead.

Which matters more than scrolling: warren *is* the terminal these agents run
in. OMP repaints a live region while it works and flushes the finished turn
up into the terminal's scrollback, so with no history of its own warren was
dropping that transcript the moment it passed the top of the screen.

**No scrolling while it works.** Mid-turn there is nothing up there to find —
the harness is repainting in place and what went past the top has not been
flushed yet — so the wheel says `AGENT BUSY` instead, and a turn starting
takes every viewer back to the live screen. Typing does too, the way it does
in any terminal.

The offset is per viewer, not per agent: the grid indexes history with
negative lines, so your phone can be reading back through a turn while the
laptop watches the live screen. A viewer reading history is shown no cursor
and is left alone by damage frames until it comes back down.

`WARREN_SCROLLBACK` sets the depth (2000 lines). It costs memory per agent
and a colony has many — though an agent on the alternate screen never puts a
line up there at all, so Claude Code agents pay nothing for it.

### Sleep mode

A colony costs what its members cost, and an idle Claude Code still holds
its several hundred megabytes. **Ctrl-Space `z`**, or **Ctrl-Z** without
leaving CLAUDE mode, stops the focused agent's claude process — and its whole process group, so tool children and MCP
servers go too — while keeping the agent itself: the daemon lives on, so the
tab keeps its row, name, color, working directory and the last screen claude
painted, dimmed under a sleeping badge. `z` again (or `Ctrl-Z`, or just typing at it)
respawns `claude --resume <session-id>` in the same burrow, and the
conversation carries on. `^Z` is deliberately taken: sent to a harness it
would be the tty's SUSP, which stops the process without telling warren —
the tab would be neither awake nor resumable, and nothing in the dashboard
could bring it back. Keys typed at a sleeping agent are buffered and
land in the resumed prompt.

The session id comes from the agent's own lifecycle hooks: Claude passes one
on stdin with every hook event, so warren knows it a second after spawn and
keeps it current. OMP agents are identified through their tty instead (see
above). Either way resuming appends to the same session, so an agent can
sleep and wake forever without ever forking its conversation.

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

This is Claude Code only; OMP agents fall back to the activity heuristic
below. warren generates a Claude Code settings file whose lifecycle hooks run
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
~/.warren/hosts             optional: ssh destinations whose agents join the sidebar
~/.warren/ssh/              ssh's shared connection sockets, one per host
```

That's everything warren writes — it only ever reads `~/.claude` and
`~/.omp`. Agents die with the machine (conversations persist in their
harness's own store and come back through the resume picker).

## Development

```sh
cargo test
```

Unit tests plus headless integration tests that spawn real daemons around
scripted children and drive them over the socket — snapshot fidelity, damage
streaming, resize fan-out, hook round-trips, exit reaping, sleep/wake (the
process really dies, the agent really doesn't), folders and two-digit
navigation on a real dashboard, and the stalled-viewer regression test.

The remote path is tested without a network: `WARREN_SSH` replaces ssh with a
stand-in that runs the command here against a second `WARREN_HOME`, so the
roster, the pipes, the socketpairs and the two-machine sidebar all run in CI.

```sh
cargo test sheep -- --nocapture     # watch the flock
```
