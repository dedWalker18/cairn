# Integrations

## Launcher search (fuzzel)

`cairn launch --find` opens a fuzzel prompt **find files ›**:
1. Type a description and press Enter.
2. The results appear in a second fuzzel list. Use <kbd>↑</kbd>/<kbd>↓</kbd> to
   move and <kbd>Enter</kbd> to open.

Opening follows the file type:
* code and text open in your editor at the matching line (`zed` by default,
  `CAIRN_EDITOR` to change it),
* everything else opens with its default app.

When a folder is recognised in the query ("…from downloads"), the list title
shows it.

Your normal app launcher stays untouched: bind file search to its own key.

**niri** (`~/.config/niri/config.kdl`, inside `binds { }`):

```kdl
Mod+Slash hotkey-overlay-title="Find files (Cairn)" { spawn "cairn" "launch" "--find"; }
```

**Sway / i3** (`~/.config/sway/config`):

```
bindsym $mod+slash exec cairn launch --find
```

**Hyprland** (`~/.config/hypr/hyprland.conf`):

```
bind = SUPER, slash, exec, cairn launch --find
```

**GNOME / KDE:** add a custom keyboard shortcut that runs `cairn launch --find`,
or `cairn` to open the app.

Also available:

* `cairn launch /mountain photos`: search directly, results in fuzzel.
* `cairn launch`: one fuzzel list with your installed apps. Picking an app
  launches it; any text that isn't an app name becomes a file search. Use this if
  you want a single key for both. Note that it replaces fuzzel's own app ordering.

If you use another dmenu-style launcher (rofi, wofi, tofi, bemenu), the same flow
works with small changes to `crates/cairn/src/launch.rs` (`fuzzel` → your
launcher's dmenu mode). PRs welcome.

## Nautilus extension

What you get:

* **Right-click on empty space in a folder → Search with Cairn…** opens the app,
  limited to that folder.
* **Right-click on a folder → Cairn ▸**
  * Search in this folder…
  * Exclude from search (removes it from the index and never looks inside again)
  * Include in search again

Install (Nautilus 43+; root is needed once because Nautilus only loads extensions
from its system directory):

```sh
sudo make -C nautilus install && nautilus -q
```

How it is built:
* It's about 200 lines of C (`nautilus/cairn-nautilus.c`). It only adds menu items
  that start Cairn's own programs. It never reads files and never talks to the
  daemon from inside Nautilus, so it cannot slow down or crash your file manager.
* It builds without `libnautilus-extension-dev`: the 46.4 headers are vendored in
  `nautilus/vendor/` (LGPL-2.1+, unchanged), and the build links the system
  `libnautilus-extension.so.4`. If the dev package is installed, its headers are
  used instead.

Check it loads the way Nautilus loads it:

```sh
make -C tests/nautilus run
```

## Scripts and AI agents

```sh
cairn search --json the invoice from march
cairn search --json --in ~/projects/acme "client SOP"
cairn search --here --json "config parser"     # current directory only
```

The JSON contains the path, category, line range, snippet, date, score and the
reasons each result matched. Agents (Claude Code, Aider, …) can use it as a
fast, local file-finding tool instead of crawling directories with `grep`/`find`.

Privacy note: snippets returned to a cloud agent leave your machine. The
contents of secret files (`.env`, keys, `*password*`) are never indexed, so they
can never appear in a snippet.

## The control socket

Each user has a socket at `$XDG_RUNTIME_DIR/cairn/cairnd.sock` (mode 0600, and
only the same uid can connect). The protocol is one JSON object per line, in and
out. See `crates/cairn-core/src/ipc.rs`.

```sh
echo '{"op":"search","query":"photos from last december","limit":5}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/cairn/cairnd.sock
```
