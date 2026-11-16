# rexa — a Linux terminal client for Rex

Rex ships a server and a CLI on Linux, but no app. `rexa` is a full-screen client that runs in
your terminal (Ghostty, Warp, …): sessions in a sidebar, tabs, split panes, a command palette,
program status (Claude Code working / waiting / done), scrollback with search, copy, image
paste, settings (中文 / English, saved machines) and a server panel. Built with Zig 0.17, libghostty-vt (the same terminal core the Rex
server uses) and libvaxis.

## Install

```sh
tools/install.sh        # builds (ReleaseFast) and installs rexa, rexa-app and the Rexa app
```

This installs `~/.local/bin/rexa` (run it in any terminal), `~/.local/bin/rexa-app` and a
**Rexa** entry with an icon in your desktop's app launcher. The app opens rexa in a window of
its own (a Ghostty window with its own app id, so it gets its own dock icon; other terminals are
used when Ghostty isn't installed). Closing the window only detaches: everything keeps
running in Rex. `rexa-app` takes the same arguments as `rexa`, e.g. `rexa-app -S ssh://box`.

Needs the pinned Zig at `~/.local/share/zig/0.17.1` (see `tools/zig`). The first build
downloads ghostty and libvaxis (pinned in `build.zig.zon`) and takes about a minute.

## Use

```sh
rexa                     # connect to the local Rex server, first session
rexa attach work         # a session by name (or ID)
rexa -S studio           # a host from `rex hosts`, a Tailscale name, …
rexa -S ssh://me@box     # any machine with Rex, over ssh
rexa ls                  # list sessions
rexa upload work f.png   # upload a file to the focused pane's machine (path gets typed)
```

### A Mac running Rex.app, over ssh

1. On the Mac: System Settings → General → Sharing → turn on **Remote Login**.
2. Here: `ssh-copy-id me@mac.local` (rexa uses ssh with `BatchMode=yes`, so it needs key
   login; a password prompt can't be answered). Check with `ssh me@mac.local rex ls`.
3. `rexa -S ssh://me@mac.local` (or `rexa-app -S ssh://me@mac.local`).

You see the Mac's sessions, the same ones Rex.app shows. rexa adds `~/.local/bin`,
`/opt/homebrew/bin`, `/usr/local/bin` and Rex.app's folder to the remote PATH, since a
non-interactive ssh shell skips `.zprofile`; if `rex` lives elsewhere, link it into one of those.
Keep the Mac awake (or turn on "wake for network access"), or the connection drops.

`-S` takes `unix:///path`, `ssh://[user@]host[:port]`, `cmd:<bridge command>`, or anything
`rex -S` understands (it is bridged through `rex __internal dial-stdio`).

## Keys

Prefix `Ctrl-b`, then:

| key | | key | |
|---|---|---|---|
| `d` | detach | `Space` `w` `:` | command palette / go to anything |
| `c` | new tab | `n` `p` `1`–`9` | next / previous / nth tab |
| `%` `"` | split right / down | `←↑→↓` `o` | move between panes |
| `z` | zoom pane | `x` | close pane |
| `[` | scrollback (then `/` search, `n` `N`, `q`) | `,` `$` | rename tab / session |
| `s` | toggle sidebar | `(` `)` `N` | previous / next / new session |
| `S` | settings (language, machines, Rex server) | `Ctrl-b` | send Ctrl-b to the program |
| `?` | key manual (also **F1**) | | |

**F1** opens the key manual from anywhere. Without the prefix: `Alt-1`…`Alt-9` tabs, `Alt-←↑→↓` panes. Everything else goes to the
program in the focused pane, through Rex's own key handling (so your `init.lua` bindings
apply).

Mouse:

- click a pane, tab, session or machine; `+` makes a tab, a session or a machine
- drag a separator between panes to resize the split
- drag selects and copies; double-click selects a word, triple-click a line
- middle-click pastes what you copied last (or the system selection)
- right-click: a menu for the pane (copy, paste, split, zoom, close…), tab (rename, close),
  session (rename, end) or machine (connect, rename, edit, delete); closing asks first
- `×` on a tab, or middle-click it, closes it; double-click renames it; the wheel over the tab
  bar switches tabs, over the sidebar switches sessions
- the status line has buttons for scrollback, the palette, settings and the key manual;
  the palette and settings take clicks too
- when the program uses the mouse itself (vim, htop…), clicks go to it; hold Shift for
  rexa's selection and menus
  (or turn on **Dragging always selects text** in Settings → General: then a plain drag
  selects even there, a click still goes to the program, and Shift-drag hands the drag to it)

Clipboard: **Ctrl-V with an image on the clipboard** uploads it to the pane's machine and
pastes its path — that is how screenshots reach Claude Code, local or remote. Files dropped on
the terminal are uploaded the same way when the server is remote. Copies made by programs
(OSC 52) land on your local clipboard.

## Settings (`Ctrl-b S`, or ⚙ in the status line)

Saved in `~/.config/rexa/config.json` (`$XDG_CONFIG_HOME/rexa/`). `Tab` or `1` `2` `3` switch pages.

- **General**: the language, 中文 or English (the first time it follows `$LANG`), and the mouse
  switch "Dragging always selects text". Both apply at once.
- **Machines**: this computer and your remote machines, each with a nickname you choose.
  `a` add (address, then name), `r` rename, `e` edit the address, `d` delete, `Enter` connect.
  Below them, **rex hosts on <the connected machine>**: that server's own list of machines.
  `Space` shows one in the machine list or hides it (also "Hide from the list" in its right-click
  menu), `s` saves it under a name. On this computer they are all shown until you choose; on a
  remote machine none are, so a Mac that itself connects back here doesn't nest. Over ssh such a
  host is reached through that machine (`ssh://me@mac#<its address there>`). The sidebar shows the
  same list: the current machine opens into its sessions, a click on another one switches to it,
  right-click for its menu. If a machine can't be reached, rexa goes back to the one you were on
  and says why.
- **Rex server**: version, process, sessions, config, Tailscale, connected clients and the
  recent log of the server you are connected to; reload the configuration (`c`), stop (`X`) or
  restart (`R`) it — both ask first, because every program running in Rex ends — and update
  (`U`: runs the package manager in a new tab, on the server's machine, so you can type your
  password). Restart works for the default local server and for ssh hosts.

## How it works

- Control: WebSocket `rex.control.v1` over the server socket (JSON requests and events).
- Each visible pane: a data connection (`rex.data.terminal.v1`) whose GHOSTSNP snapshot is
  decoded by libghostty, then live output; rexa renders the libghostty screen with vaxis.
- Keys go to `client.key` (the server encodes them for the pane's keyboard mode); pastes to
  `block.event`; sizes via `own_block` + `resize` ("whoever types decides the size").
- Dropped connections reconnect by themselves (control resume token, data RESUME).
- Focus is the server's shared focus, like tmux: switching tabs here switches them for
  Rex.app on a Mac too.

## Development

```sh
tools/zig build test                         # unit tests
tools/zig build -Doptimize=ReleaseFast       # zig-out/bin/rexa
REXA_DEBUG=/tmp/rexa.log rexa                # log every handled event
```

Test against an isolated server, never your real one:

```sh
rex server run --listen unix:///run/user/$UID/rexa-test.sock --state-dir /tmp/rexa-state --restore=false &
rexa -S unix:///run/user/$UID/rexa-test.sock
```
