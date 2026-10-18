# Installing Cairn

## Requirements

| | Minimum | Why |
|---|---|---|
| Linux kernel | 5.13+ (6.7+ recommended) | Landlock sandbox; 6.7 adds the network deny |
| Rust | 1.85+ | build |
| GTK | 4.12+ | the `cairn` app (`cairn-gui`) |
| Nautilus | 43+ (optional) | right-click menu |
| poppler-utils | any (optional) | text from PDFs |
| fuzzel | 1.9+ (optional) | launcher search |
| Disk | ~1 GB + index (≈ 1–6 GB for a large home) | models + index |
| RAM | 4 GB+ | indexing peaks at ~1.5 GB, idle ~300 MB |

Any filesystem works. btrfs and ext4 are tested.

## Build and install (per user, no root needed)

**Debian / Ubuntu**

```sh
sudo apt install build-essential pkg-config libgtk-4-dev poppler-utils curl
```

**Fedora**

```sh
sudo dnf install gcc pkgconf-pkg-config gtk4-devel poppler-utils curl
```

**Arch**

```sh
sudo pacman -S base-devel gtk4 poppler curl
```

Then:

```sh
curl --proto '=https' -sSf https://sh.rustup.rs | sh   # if needed
git clone https://github.com/dedWalker18/cairn.git && cd cairn
./install.sh
```

`install.sh` builds the release binaries and installs:

| What | Where |
|---|---|
| `cairnd` (daemon), `cairn` (CLI + app launcher), `cairn-gui` (app) | `~/.local/bin/` |
| systemd user unit | `~/.config/systemd/user/cairnd.service` |
| desktop entry "Cairn" | `~/.local/share/applications/` |

Make sure `~/.local/bin` is on your `PATH`.

## Models (one time)

```sh
cairn models fetch        # downloads ~640 MB from Hugging Face, verifies SHA-256 of every file
cairn models verify
```

No network? See [AIRGAP.md](AIRGAP.md). What the models are, and what was
changed for speed: [MODELS.md](MODELS.md).

## Start

```sh
systemctl --user enable --now cairnd
cairn status       # progress
cairn              # the app
```

Cairn indexes your home folder. It skips hidden folders, `node_modules`,
virtualenvs, other drives and anything containing a `.noindex` file. To keep
other folders out:

```sh
cairn exclude ~/datasets          # or: touch ~/datasets/.noindex
```

What to expect on a large home (~500k files, laptop CPU):

| Stage | Takes about |
|---|---|
| Scan | a minute |
| Name and date search | minutes |
| Meaning search over text | hours of idle time |
| Photos | after text |

## Optional integrations

* **Nautilus menu** (needs root once: Nautilus only loads extensions from a system directory):
  ```sh
  sudo make -C nautilus install && nautilus -q
  ```
* **Launcher search** (fuzzel, niri, Sway, Hyprland, GNOME, KDE): see [INTEGRATIONS.md](INTEGRATIONS.md).

## Update

```sh
git pull && ./install.sh && systemctl --user restart cairnd
sudo make -C nautilus install && nautilus -q      # if you use the extension
```

The index upgrades itself:
* schema changes migrate in place, or rebuild in the background,
* a model change re-embeds in the background.

Your exclusions are always kept.

## Uninstall

```sh
systemctl --user disable --now cairnd
rm ~/.local/bin/{cairnd,cairn,cairn-gui} ~/.config/systemd/user/cairnd.service \
   ~/.local/share/applications/dev.cairn.Search.desktop
rm -rf ~/.local/share/cairn          # index + models
sudo make -C nautilus uninstall      # if installed
```

Cairn never wrote to your files, so there is nothing else to clean up.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `cairn: connecting to … (is cairnd running?)` | `systemctl --user start cairnd`; `journalctl --user -u cairnd -e` |
| Index page says "Resting · on battery" | By design. Plug in, or set `CAIRN_RUN_ON_BATTERY=1` (below) |
| Meaning search finds nothing yet | Still learning: check `cairn status` (EmbedText / ImageEmbed pending) |
| PDFs have no text hits | Install `poppler-utils` |
| Nautilus menu missing | `sudo make -C nautilus install`, then `nautilus -q` |
| Everything | `cairn doctor` checks Landlock, models, pdftotext, the daemon |

### Environment variables (set in the systemd unit with `systemctl --user edit cairnd`)

| Variable | Effect |
|---|---|
| `CAIRN_ROOTS=/a:/b` | index these folders instead of `$HOME` |
| `CAIRN_RUN_ON_BATTERY=1` | keep indexing on battery |
| `CAIRN_LOG=debug` | more logging (journal only) |
| `CAIRN_START_PAUSED=1` | start with indexing paused |
| `CAIRN_EDITOR=/usr/bin/code` | editor for code hits (default: `zed` if found) |
| `CAIRN_DATA_DIR`, `CAIRN_MODELS_DIR`, `CAIRN_RUNTIME_DIR` | relocate data, models, socket |
