# Running Cairn on an airgapped machine

Cairn needs no network at runtime. The daemon contains no network code, and
two independent mechanisms forbid network access anyway:

1. **Landlock** (kernel): the daemon and every worker deny all TCP bind and
   connect. This needs Landlock ABI v4+ (Linux 6.7+). On older kernels the
   filesystem rules still apply.
2. **systemd**: `RestrictAddressFamilies=AF_UNIX` in `cairnd.service`. Only Unix
   sockets can be created.

The only command that ever uses the network is `cairn models fetch`, and you
never run it on the airgapped machine.

## 1. Build offline

On a connected machine:

```sh
git clone <repo> cairn && cd cairn
cargo vendor --locked vendor > .cargo-vendor-config.toml
mkdir -p .cargo && cat .cargo-vendor-config.toml >> .cargo/config.toml
tar czf cairn-src.tgz .
```

The `ort` crate downloads a prebuilt ONNX Runtime static library during the
build (D-061). For an offline build, download the matching ONNX Runtime 1.31
static build on the connected machine, copy it over, and set
`ORT_LIB_LOCATION=/path/to/onnxruntime/lib` before `cargo build`. The
resulting binaries contain ONNX Runtime and need nothing extra at runtime.

On the airgapped machine (Rust toolchain installed from your offline mirror):

```sh
tar xzf cairn-src.tgz && cd cairn
cargo build --release --offline -p cairnd -p cairn -p cairn-gui
make -C nautilus           # needs gcc + glib/gio dev headers (libgtk-4-dev pulls them)
```

Runtime dependencies are all standard distribution packages: glibc, GTK 4.8+
(GUI only), `libnautilus-extension.so.4` (extension only), and `pdftotext`
from poppler-utils (PDF text; optional). SQLite is compiled in.

## 2. Move the models

On a connected machine:

```sh
cairn models list            # URLs, sizes and SHA-256 of every file
# or simply:
cairn models fetch           # then copy ~/.local/share/cairn/models/
```

Copy the files to the airgapped machine and install them:

```sh
cairn models install /media/usb/cairn-models
#   accepts <dir>/<model-id>/<file> or <dir>/<file>
#   every file is checked against the SHA-256 pinned in the source; a mismatch is refused
cairn models verify
```

| Model | Files | Size | License |
|---|---|---|---|
| bge-small-en-v1.5 (BAAI; int8 ONNX export from Xenova/bge-small-en-v1.5) | tokenizer.json, model_int8.onnx | 35 MB | MIT |
| clip-vit-b32-laion2b (LAION) | config.json, tokenizer.json, model.safetensors | 607 MB | MIT |

Without models, Cairn still indexes names, dates and full text. Semantic and
image search activate when the models appear (they are re-checked every 10
minutes; restart the daemon to load the query side).

## 3. Verify the guarantees yourself

```sh
cairn doctor                                   # Landlock ABI, models, pdftotext, daemon
cairn status                                   # "sandbox: landlock: Full (writes only to …)"
systemctl --user show cairnd -p RestrictAddressFamilies
ss -xp | grep cairnd                           # only unix sockets
```
