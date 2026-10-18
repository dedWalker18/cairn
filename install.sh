#!/usr/bin/env bash
# Per-user install. Touches only ~/.local/bin, ~/.local/share and
# ~/.config/systemd/user. The Nautilus extension step needs root and is
# printed, not run.
set -euo pipefail
cd "$(dirname "$0")"
missing=()
command -v cargo >/dev/null || missing+=("Rust (https://rustup.rs)")
command -v cc >/dev/null || missing+=("a C compiler (build-essential / gcc)")
pkg-config --exists gtk4 2>/dev/null || missing+=("GTK 4 development files (libgtk-4-dev / gtk4-devel / gtk4)")
if [ ${#missing[@]} -gt 0 ]; then
  printf 'Missing: %s\n' "${missing[@]}"; echo "See docs/INSTALL.md"; exit 1
fi
command -v pdftotext >/dev/null || echo "note: pdftotext (poppler-utils) not found; PDFs will be searchable by name only"
cargo build --release -p cairnd -p cairn -p cairn-gui
make -C nautilus
install -d -m 0755 "$HOME/.local/bin" "$HOME/.config/systemd/user" "$HOME/.local/share/applications"
for b in cairnd cairn cairn-gui; do install -m 0755 "target/release/$b" "$HOME/.local/bin/$b"; done
install -m 0644 packaging/systemd/cairnd.service "$HOME/.config/systemd/user/cairnd.service"
install -m 0644 packaging/desktop/dev.cairn.Search.desktop "$HOME/.local/share/applications/"
systemctl --user daemon-reload
"$HOME/.local/bin/cairn" models verify >/dev/null 2>&1 || echo "models missing: run 'cairn models fetch' (online) or 'cairn models install <dir>' (offline)"
cat <<MSG

Installed. Next steps:
  1. systemctl --user enable --now cairnd        # start indexing (idle priority)
  2. sudo make -C nautilus install && nautilus -q # Nautilus menu (needs root once)
  3. cairn                                         # open the app
  4. optional launcher key: see docs/INTEGRATIONS.md (cairn launch --find)
MSG
