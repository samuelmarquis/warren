# warren

A terminal dashboard for running many **Claude Code** agents at once over SSH.

A warren is a maze of interconnected burrows where a colony lives — and
*survives*. Each agent gets its own burrow ([abduco](https://github.com/martanne/abduco)
session); you tunnel between them through a left-hand sidebar (a patched
[dvtm](https://github.com/martanne/dvtm)); and the whole thing outlives the SSH
connection it was started in.

```
┌────────────────┬───────────────────────────────────────────┐
│ > refactor-db  │  ● Claude Code                              │
│   web-ui       │                                             │
│   infra-tf     │  > running tests…                           │
│   scratch      │  ✔ 42 passed                                │
│                │                                             │
│   + new agent  │  > _                                        │
└────────────────┴───────────────────────────────────────────┘
   ↑ sidebar: every agent           ↑ the focused agent's live TUI
```

- **Left column** — one row per agent (its name), plus a pinned `+ new agent`
  tab at the bottom. The focused agent is highlighted; tab colors are optional.
- **Right pane** — the focused agent's actual Claude Code TUI, full screen.
- **Survives disconnects** — drop your SSH session (cleanly *or* by yanking the
  cable) and every agent keeps running. Reconnect and run `warren` to drop right
  back in.

## How it's built

Two layers of abduco, plus a patched dvtm as the renderer:

```
ssh ── warren-abduco (dashboard session)        ← survives the SSH drop
          └── warren-dvtm  (sidebar + fullscreen)
                ├── warren-abduco -a <agent-1>   ← each agent is its own
                │     └── claude                    independent session…
                └── warren-abduco -a <agent-2>
                      └── claude
```

The "+ new agent" tab and the rename/edit screens are **native dvtm
screens** (warren's `SUB_NEW` / `SUB_EDIT`), not separate programs — the
pinned tab just shows a hint and `Enter` opens the form in the pane.

- **Per-agent sessions** live in `~/.warren/agents/` and are completely
  independent — an agent survives even if dvtm itself crashes, and can be
  attached on its own with `warren-abduco -a <name>` (sockets in that dir).
- **The dashboard session** lives in `~/.warren/dash/`. When you run `warren`
  it reattaches if one is alive, otherwise it creates one and **discovers**
  every existing agent, opening a window for each.
- The two live in separate socket dirs so discovery never trips over the
  dashboard itself.

## Repository layout

This repo vendors its two patched dependencies as **nested git repos** (not
submodules): `abduco/` and `dvtm/` are clones of their upstreams with our changes
committed on a `warren` branch — upstream `master` is left untouched, so the whole
patch set for each is `git log master..warren`. warren's own repo ignores those
two directories; `install.sh` builds both in place and installs everything to
`~/.local/bin`.

- `dvtm/` — clone of `martanne/dvtm`. Our configuration lives in the tracked
  `config.def.h` (the build copies it to the gitignored `config.h`), so a clean
  checkout builds with the warren config.
- `abduco/` — clone of `legionus/abduco` (a fork that adds screen-replay on
  reattach).

## Install

```sh
cd ~/Developer/warren && ./install.sh   # builds the patched dvtm + abduco
```

Installs five programs into `~/.local/bin` (override with `PREFIX=…`):
`warren`, `warren-sessions`, `warren-hook`, `warren-dvtm`, `warren-abduco`.
Make sure `~/.local/bin` is on your `PATH`.

> **ncurses:** dvtm builds against **Homebrew's ncurses 6** if present
> (`brew install ncurses`) — needed for SGR mouse, i.e. trackpad/wheel scrolling
> and mouse past column 223. macOS's system ncurses is 5.x (X10 mouse only, no
> wheel); the build falls back to it but you lose wheel scrolling. The Homebrew
> dylib is a runtime dependency, so keep ncurses installed.
>
> Build notes (macOS): dvtm/abduco need `-D_DARWIN_C_SOURCE` (for `SIGWINCH`),
> and dvtm's `send()` action was renamed to avoid clashing with the socket
> `send()`. These are baked into the checked-out sources.

## Use

```
warren                 launch the dashboard, or reattach if it's already running
warren new NAME [DIR] [COLOR 0-6] [new|resume|continue]   create an agent
warren ls              list agents and their status
warren kill NAME       terminate an agent
warren help
```

Inside the dashboard there are two modes, vim-style. You start in **INSERT**
(keystrokes go to the focused Claude window). `Ctrl-Space` enters **NORMAL**
mode, where the sidebar is driven with single keys; `i`/`a`/`Enter`/`Esc` (or
`Ctrl-Space` again) return to INSERT. A bottom status line always shows the
current mode and a cheatsheet.

The sidebar is a set of numbered **slots**. Agents fill the lowest free slots;
there's no permanent "+ new agent" row — instead, an empty slot *is* the new
agent screen (focus one and the create form appears in the pane).

**NORMAL mode keys:**

| key | does |
|-----|------|
| `j` / `k` (or ↓/↑) | move down / up the sidebar (focus follows, live preview) |
| `1`–`9`, `0` | jump to the 1st–10th slot; an empty slot opens the new-agent form |
| `Shift`+`1`–`9`,`0` (`!@#$%^&*()`) | swap the focused agent with the agent in that slot |
| `n` | jump to the lowest free slot (= new agent) |
| `l` (or →) | step into the focused Claude pane (= INSERT) |
| `g` / `G` | jump to first / last |
| `Ctrl-u` / `Ctrl-d` | scroll the pane half a page up / down (no Page keys needed) |
| `r` | rename the selected tab (quick inline prompt; persists, and pins the name) |
| `c` | open the **color-grid picker** (arrow around all 256 colors; Enter picks) |
| `e` | edit form for the selected agent — change **name + color** together (persists) |
| `x` | close the selected agent (terminates it; `y`/`n` confirm) |
| `:` | command line — `:q!` quits warren; `:color`/`:c` sets the tab color (below) |
| `i` `a` `Enter` `Esc` | back to INSERT (type into Claude) |
| `Ctrl-Space` | toggle back to INSERT |

`hjkl` mirror the arrow keys throughout — including the create/edit forms, where
they move between fields and change the Mode chooser (on the Name/Root-dir/Color
text fields they type normally; use Tab or ↑/↓ to leave a text field). The green
badge in the bottom status line is the NORMAL-mode indicator.

### Tab colors

A tab color is any **xterm-256 color** (`1`–`255`; `0`/empty = none). Set it three
ways:

- **`c`** in NORMAL mode — a **grid picker**: a 16×16 swatch grid of all 256
  colors (plus a "none" cell). Arrow / `hjkl` around it, `Enter` to pick, `Esc` to
  cancel; the header shows the hovered index and `#hex`.
- **`:color <c>`** (or **`:c <c>`**) — takes a `#hex` (quantized to nearest 256)
  or a `0`–`255` index, e.g. `:c #ff8700`, `:color 208`.
- the **Color** field in the `e` edit / new-agent form — same hex/index, with a
  live swatch.

The focused tab is shown as a solid bar in its color, with **black or white text
chosen automatically** by the color's lightness so it stays readable.

**Names follow Claude.** The sidebar mirrors whatever Claude is calling each
session — it sets its terminal title to a live summary of the current task (e.g.
"Summarize the readme"), and warren adopts that as the agent's name, persisting
it like a rename. The creation-time name shows until Claude names the session;
the leading spinner glyph and the idle "Claude Code" default are filtered out, so
the last meaningful title latches without flicker. A manual `r`/`e` rename sticks
until Claude's title next changes.

### Working vs. ready-for-input

The sidebar tells you, at a glance, which agent needs you. Each agent row is:

| look | meaning |
|------|---------|
| **dim**, no marker | **working** — a turn or tool is in progress |
| **bold**, trailing `*` | **waiting** for input, and you haven't looked since it went idle |
| **bold**, no marker | waiting for input, already examined (the `*` clears when you focus it) |
| **bold**, trailing `!` | **blocked on a permission prompt** — it can't proceed until you answer |

This works for *every* agent, including the ones you aren't looking at, so you
can fire off several and watch the markers light up as each one finishes or asks
for something. The `*` is an unread flag: it appears when an agent goes idle
**while unfocused**, and clears the moment you focus it (so a row you've already
checked stays bold but quiet, and the `*` returns only if it works and stops
again).

State comes from **Claude Code lifecycle hooks**, which warren wires into each
agent automatically via `claude --settings ~/.warren/hooks.json` (a generated
file; it does not touch your global or project Claude config). The hooks
(`UserPromptSubmit`/`PreToolUse` → working, `Stop` → waiting, `Notification` →
attention, etc.) run `warren-hook`, which records the state to
`~/.warren/meta/<agent>.state`; dvtm polls those files. The key win over guessing
from the screen: the hooks keep an agent marked **working through a silent tool
run** (a quiet `npm install`, a long test) that no screen-watching heuristic
could tell apart from "done".

If the hooks ever don't report (a non-Claude agent, an old build), warren falls
back to a **repaint-activity heuristic**: Claude's working status line repaints
its elapsed-seconds counter ~once a second, so an agent is treated as "ready"
once its screen has been quiet for `WARREN_BUSY_IDLE_MS` (default 1500ms, in
`dvtm/config.h`). That fallback is deliberately verb-agnostic — it never matches
Claude's randomised spinner words ("Cogitating…", "Churned for 47s", …), so it
survives version changes. (While any agent is working, dvtm wakes ~4×/second to
re-check; when everything is idle it blocks with zero wakeups.)

### Scrolling

Two-finger trackpad scroll (a mouse wheel) over a pane scrolls back through that
agent's output, in either mode — and `Ctrl-u`/`Ctrl-d` do the same from the
keyboard. Typing returns to the live view. This needs a modern ncurses for the
wheel (see Install) and works because agents render on the normal screen
(`ABDUCO_NO_ALTSCREEN`), so dvtm keeps their scrollback.

Mouse still works in both modes: **click a sidebar row** to focus it, wheel
over the sidebar to move. **`Ctrl-\`** detaches the whole dashboard at any time
(agents keep running; reattach with `warren`).

The **create form** (`+ new agent`) is a small tab-through form. The first
field, **Mode**, chooses how the agent starts:

| mode       | runs                  | effect                                       |
|------------|-----------------------|----------------------------------------------|
| `new`      | `claude`              | a fresh session in the chosen **Root dir**   |
| `resume`   | `claude --resume <id>`| **import any past session** — see below      |
| `continue` | `claude --continue`   | most recent conversation in **Root dir**     |

- `Tab` / `Shift-Tab` move between fields; `←` / `→` (or `Space`) change a
  chooser (Mode / Color); type to edit text (Title / Root dir).
- `Enter` creates the agent and jumps to it; `Esc` clears the form.

**Importing a pre-existing session (resume):** warren reads your sessions
directly from `~/.claude/projects` (via `warren-sessions`) and shows a **global
picker across every project** — not just one directory. Set Mode to `resume`,
`↑`/`↓` to pick a session (each row shows *when · project dir · title*), `Enter`
to resume it. The agent launches `claude --resume <id>` **in that session's own
working directory**, regardless of where warren was started, and is named after
the project. Discovery is decoupled from `--resume`'s own picker, so it isn't
limited to the current dir and won't break if that picker's behavior changes.

From the CLI: `warren sessions` lists them; `warren new mywork ~/proj 4 resume <id>`
resumes a specific one.

Reach the form from NORMAL mode with `n` (or move to the `+ new agent` row and
press `i`/`Enter`). Closing an agent is `x` in NORMAL mode (or just quit Claude
inside it with `Ctrl-C` / exit) — the row disappears from the sidebar.

Renames (`r`) and colors (`c`) are remembered in `~/.warren/meta` and reapplied
when you relaunch.

### Mouse

Clicking a sidebar row focuses that agent; the wheel over the sidebar moves the
selection, and the wheel over a pane scrolls that agent's output. This needs the
**SGR mouse** support in ncurses 6 (Homebrew) — see Install. With the macOS
system ncurses (5.x, X10 only) clicks still work for the sidebar but the wheel
does not, so use `Ctrl-u`/`Ctrl-d` to scroll.

### Clipboard & text selection

**Copying from inside an agent works over SSH.** When Claude copies something
(e.g. its `c` "copy link"), it emits an **OSC 52** clipboard escape; warren
forwards that from the focused agent straight to your real terminal, so the text
lands on your **local** machine's clipboard (not the mac mini's). iTerm2 must have
*Preferences → General → Selection → "Applications in terminal may access the
clipboard"* enabled (it may prompt the first time). Only the **focused** agent can
set the clipboard, so a background agent can't clobber it.

To **select text with the mouse** (warren grabs the mouse for the sidebar/wheel,
which otherwise blocks native selection): in iTerm2 hold **⌥ Option** and drag to
make a normal selection that copies on release. That's an iTerm2 feature, not a
warren mode — it bypasses mouse reporting for that drag.

## Configuration

Environment variables (all optional):

| var                  | default                  | meaning                              |
|----------------------|--------------------------|--------------------------------------|
| `WARREN_HOME`        | `~/.warren`              | base dir for sessions + metadata     |
| `WARREN_AGENT_CMD`   | `claude`                 | command each agent runs              |
| `WARREN_DASH_NAME`   | `dashboard`              | dashboard session name               |
| `WARREN_ABDUCO` / `WARREN_DVTM` | next to `warren` | the patched binaries        |
| `DVTM_TERM`          | `dvtm-256color` if installed, else `screen-256color` | `TERM` inside windows |

Sidebar width, colors, and key bindings live in `dvtm/config.h` (recompile to
change). The agent list column is `SIDEBAR_WIDTH` (default 24).

## What was patched

- **dvtm** (`dvtm/dvtm.c`, `dvtm/config.h`): a fixed-width left sidebar that
  lists every window by a stable name with optional tab color; reserve those
  columns in `updatebarpos()`; force the fullscreen layout so the focused agent
  owns the right pane; click/wheel sidebar handling; treat an empty cwd as "no
  chdir".
- **dvtm** (modal UI): a vim-ish two-mode input scheme handled directly in the
  main loop — `Ctrl-Space` toggles NORMAL mode; `hjkl`/arrows navigate, `1-9/0`
  select numbered slots, `Shift`+digit (`!@#…`) swaps slots, `r`/`c`/`e` rename
  & recolor, `n`/empty-slot create, `x` close, `Ctrl-u`/`Ctrl-d` scroll,
  `i/a/Enter/Esc` back to INSERT. Numbered **slots** with the create form as
  "chrome" shown only when focused; new agents append to the lowest free slot.
  Renames/colors persist to `~/.warren/meta` (dvtm writes them directly using
  the agent's session name, passed as a 5th `create` arg, and via the `rename`/
  `recolor` cmd-FIFO commands from the edit form); `x` shells out to `warren kill`.
- **dvtm + warren** (working/waiting/attention indicator): agents report their
  state through **Claude Code hooks**. `bin/warren-hook` (run by the hooks)
  writes `working`/`waiting`/`attention` to `~/.warren/meta/<agent>.state`, which
  the launcher wires in per-agent via `claude --settings ~/.warren/hooks.json`
  (generated by `ensure_hooks_file`; the agent is tagged with `WARREN_AGENT` so
  the hook knows which file to write). dvtm polls the state files and draws each
  agent **dim** (working), **bold + `*`** (idle and unexamined — `*` clears on
  focus), **bold** (idle, examined), or **bold + `!`** (blocked on a permission
  prompt). The hooks keep an agent "working" through a *silent* tool run that no
  screen heuristic can see. If no hook state is present, dvtm falls back to a
  verb-agnostic **repaint-activity heuristic** (`Client.last_activity` vs
  `WARREN_BUSY_IDLE_MS`). The main `pselect` gains an adaptive timeout (poll
  ~4×/s while any agent is working, block when all idle) so the working→idle flip
  — and silent state-file changes — are caught even for unfocused agents.
- **dvtm** (name follows Claude): `term_title_handler` mirrors Claude's terminal
  title into the agent's sidebar name (`warren_sync_name`), stripping the leading
  spinner glyph and ignoring the idle `WARREN_IDLE_TITLE` ("Claude Code") so the
  last real title latches; it persists like a rename. The focused-row `>` marker
  was dropped (the highlight already shows selection).
- **dvtm** (`dvtm/vt.c`): OSC strings (e.g. window titles) now accumulate
  non-ASCII as **UTF-8** instead of truncating each `wchar_t` to one byte — that
  truncation turned Claude's "✳ Claude Code" title into "3 Claude Code". The fix
  is scoped to OSC (`ebuf[0]==']'`), so CSI and other sequences are byte-identical.
- **dvtm** (`dvtm/vt.c`): 24-bit truecolor (`SGR 38;2;R;G;B` / `48;2;R;G;B`) is
  **quantized to the nearest xterm-256 color** (`vt_rgb_to_256`, exposed in
  `vt.h`; the standard tmux cube-vs-gray mapping) rather than dropped. dvtm
  renders through ncurses 256-color pairs and has no true 24-bit path; this lets
  apps emit truecolor and get a faithful 256-color approximation. (The colon form
  `38:2:…` is not parsed — dvtm tokenizes CSI params on `;` only — but Claude/iTerm
  use the `;` form.)
- **dvtm + warren** (full-range tab colors): a tab color is now any xterm-256
  index (`1`–`255`; `0` = none), not a fixed 7-entry palette. ncurses pairs are
  reserved **eagerly at startup** per color (`sidebar_fg`/`sidebar_sel`) — doing it
  lazily at runtime made `vt_color_reserve` grab a pair number `vt_color_get` had
  already handed to agent output, so **Claude's text rendered in the tab's color**;
  reserving up front (like the original code) avoids that collision. But eager
  reservation pushes pair numbers past 255, and the 8-bit **`COLOR_PAIR()`** macro
  *truncates* the pair number (`226 & 0x7F = 98`) — so high colors rendered as the
  wrong color (yellow showed up as a soft purple). The sidebar and grid therefore
  set the pair with **`attr_set(attrs, pair, NULL)`**, which takes the full pair
  number, instead of `attrset(COLOR_PAIR(...))`. The focused row is a solid color
  bar with **black/white text chosen by Rec.601 luminance** (`color_is_dark`).
  Colors are set by the `c` grid picker (`SUB_COLOR`: a 16×16 swatch grid with
  `hjkl`/arrow nav), `:color`/`:c <#hex|index>`, or the edit/new form's hex field
  — all via `warren_parse_color` (reusing `vt_rgb_to_256`).
- **dvtm** (`:` command-line editing): the status-bar command line
  (`handle_cmd_key`) is a real one-line editor with a movable cursor
  (`warren_cmd_cursor`) — ←/→, `Ctrl-A`/`Ctrl-E` (home/end), `Delete`, and
  mid-string insert / backspace-before-cursor; the status bar draws a block cursor
  at the edit position. A lone `Esc` still cancels, disambiguated from arrow-key
  escape sequences by `ESCDELAY=100`.
- **dvtm** (rename pins the name): the Claude-title auto-sync (`warren_sync_name`)
  was overwriting manual renames every frame (Claude re-emits its title
  continuously). A manual `r`/`e` rename now sets `Client.name_pinned`, which the
  sync respects; the pin is persisted (`<agent>.namepin`, a 6th `create` arg) so
  it survives a dashboard restart.
- **dvtm** (`dvtm/vt.c`, `view_offset`): **scroll-and-stay scrollback.** A window
  scrolled back keeps a stable view while live output keeps flowing below it
  (iTerm-like), instead of freezing the pane or snapping to the bottom on every
  newline. The fix decouples user scroll from the live write target: `b->lines`
  is now *always* the live screen (the producer's cursor never moves on user
  scroll); user scroll is a read-only `view_offset` and `vt_draw` renders the
  scrolled window from the ring iterators, so in-place spinner/status output
  lands off-window and can't corrupt the view. A real newline while scrolled
  banks to history and bumps `view_offset` in lockstep, pinning the view to its
  content. `vt_scroll` is split from a new `vt_app_scroll` (CSI S/T = SU/SD) so
  apps that scroll their own content no longer falsely trip the scrollback state.
  Replaces the earlier freeze-while-scrolled mitigation (the two `draw_content`
  draw-gates are gone). Covered by `dvtm/t_vt.c` (a libvt harness, 46 checks).
- **scrollback depth** (`bin/warren`, abduco `-L`): an agent's scrollback lives
  in the *dashboard's* dvtm window, which is (re)populated from abduco's replay
  buffer when the relay (`abduco -a <agent>`) attaches. abduco defaults that
  buffer to **120 lines** (`screen_max_rows`), which walled scrolling off just
  above where the dashboard attached. warren now spawns agents with
  `-L $WARREN_SCROLLBACK` (default **5000**, matching dvtm's `SCROLL_HISTORY`) so
  the full history replays. Applies to newly-spawned agents; existing sessions
  keep their 120-line buffer until recreated.
- **fullscreen agents** (`dvtm`, wheel forwarding): the above scrollback is for
  *inline* Claude (`tui: "default"`). Claude's **fullscreen** renderer
  (`tui: "fullscreen"`) draws on the terminal's **alternate screen**, which has
  no scrollback by design — there, scrolling is Claude's own. So dvtm routes the
  wheel by screen: normal screen → dvtm scrollback; **alt screen → forward the
  wheel to the app** (`vt_mouse_wheel`) so Claude's fullscreen scroll works.
  dvtm tracks the app's mouse modes (DECSET 1000/1002/1003 + SGR 1006) and only
  forwards when the app is listening, in the encoding it asked for.
- **dvtm** (`dvtm/vt.c`, OSC 52 clipboard): the vt now forwards an **OSC 52**
  clipboard write (`vt_clipboard_handler`) from the *focused* agent to the real
  terminal fd, instead of dropping it in the `default` OSC case. That's how
  Claude's `c` "copy link" reaches the host clipboard over SSH (both abduco layers
  + iTerm2 are byte-transparent for it). Gated to the focused window so a
  background agent can't hijack the clipboard.
- **dvtm** (attention vs. running tool): the `!` marker (a `Notification` hook)
  is sticky — it stays set while an approved tool runs (nothing fires until
  `PostToolUse`). So `!` now shows only when the hook says `attention` **and** the
  screen is quiet (`agent_busy` false); a still-repainting agent is mid-tool and
  shows as working, not blocked-on-you.
- **dvtm** (`dvtm/vt.c` + `config.mk`): wheel / `Ctrl-u`-`d` scroll the focused
  window's scrollback (`vt_scroll`); built against **Homebrew ncurses 6** for SGR
  mouse so the trackpad wheel is decoded (system ncurses 5.x can't).
- **dvtm** (`dvtm/vt.c`): ignore CSI sequences with a private-parameter prefix
  (`<`,`=`,`>`). `ESC[>4m` (xterm modifyOtherKeys, emitted by Claude Code) was
  being misparsed as SGR 4 = underline, underlining everything Claude drew (and
  re-triggering on Ctrl-L). **Fix for the "everything underlined" corruption.**
- **warren** (`bin/warren`, `spawn_agent`): run agents with `TERM=$DVTM_TERM`
  (screen-256color, matching what dvtm's vt emulates) and **drop `COLORTERM`**.
  Agents otherwise inherited the outer `xterm-256color`, under which curses apps
  like `vis` quantize 256-color themes to the wrong (dark) palette; and
  `COLORTERM=truecolor` (set by iTerm2, forwarded over SSH) made Claude emit
  24-bit-color SGR that dvtm's vt doesn't parse. **Fix for vis's wrong colors.**
- **warren** (`bin/warren`): create `$WARREN_AGENTS_DIR`/`$WARREN_DASH_DIR` up
  front. abduco's `create_socket_dir` does a *non-recursive* `mkdir
  "$ABDUCO_SOCKET_DIR/abduco"` and silently falls back to a shared dir
  (`$TMPDIR/abduco`, `/tmp/abduco/$USER`, `~/.<name>`) if the parent is missing —
  so without this every `WARREN_HOME` shared one agent pool and sockets escaped
  `~/.warren`. Creating the dirs first keeps each home isolated and on disk.
- **dvtm** (`dvtm/vt.c`): treat a pty EOF (`read` → `0`) as window death. On
  Linux a dead pty master reads `-1/EIO`, but on macOS/BSD it reads `0`; without
  this, an exiting window (e.g. a resume picker cancelled with Esc) left dvtm
  spinning on a permanently-readable fd — freezing the tab, starving `SIGCHLD`,
  and leaving a zombie. **This is the fix for the frozen-tab/dead-dashboard bug.**
- **abduco** (`abduco/client.c`): an ungraceful terminal loss (`SIGHUP`, stdin
  `EIO`, `pselect` error) now performs a *graceful detach* instead of crashing
  with an I/O error, so the session is always left running for reattach. Also
  `ABDUCO_NO_ALTSCREEN=1` (set by warren for agents) skips the alternate-screen
  switch, so agents render on the normal screen and dvtm keeps their scrollback.
- **abduco** (`abduco/server.c`): the screen-replay buffer (this fork's
  reattach feature) sent each buffered "line" in one packet, but a full-screen
  TUI redraw is one long run with no newline — far bigger than the 4 KB packet
  buffer — so the `strncpy` overflowed and crashed the server on reattach
  (`dashboard: exited due to I/O errors`). Now it chunks long content into
  packet-sized pieces and caps per-line growth. **This is the fix for the
  reattach-after-filling-the-screen I/O error.**
- **warren-sessions**: enumerates Claude sessions straight from
  `~/.claude/projects/*/*.jsonl` (id, cwd, title, mtime) so the resume picker
  spans every project and doesn't depend on `claude --resume`'s own UI.
- **warren-hook**: tiny script run by each agent's Claude Code lifecycle hooks
  (wired in via `claude --settings`) that records the agent's
  working/waiting/attention state to `~/.warren/meta/<agent>.state` for the
  sidebar. A silent no-op for any Claude run outside warren.
