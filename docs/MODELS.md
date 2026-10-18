# Models

Cairn uses two small open models, both MIT-licensed. Neither is bundled in this
repository. `cairn models fetch` downloads them from Hugging Face at **pinned
revisions**, and every file is checked against a SHA-256 hash pinned in the
source (`crates/cairn-ml/src/lib.rs`) before it is ever loaded. A mismatch is
refused.

There is no generative model (LLM) anywhere in Cairn. Query understanding (dates,
kinds, places) is a small deterministic grammar (`crates/cairn-core/src/query.rs`).

| Purpose | Model | Size | Runtime | License |
|---|---|---|---|---|
| Text: documents, code, notes, folder context, queries | [BAAI/bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5), int8 ONNX export from [Xenova/bge-small-en-v1.5](https://huggingface.co/Xenova/bge-small-en-v1.5) | 33M params, 35 MB | ONNX Runtime (statically linked) | MIT |
| Images, plus text→image for queries | [laion/CLIP-ViT-B-32-laion2B-s34B-b79K](https://huggingface.co/laion/CLIP-ViT-B-32-laion2B-s34B-b79K) | 151M params, 605 MB | [candle](https://github.com/huggingface/candle) (pure Rust) | MIT |

## Changes we made, and why

No weights were retrained or fine-tuned. The changes are in how the models are
run.

### 1. Text model runs as int8 on ONNX Runtime

* **Before:** candle (pure Rust) with fp32 weights. On a laptop CPU it ran at
  ~0.8 chunks/s: candle's element-wise ops (GELU, attention masks, the generic
  softmax) are single-threaded and dominated the time.
* **Step 1:** vendored the BERT module with the fused `softmax_last_dim` kernel,
  and cut sequences from 512 to 256 tokens. That gave 3.6 chunks/s, still days
  for a large home.
* **Now:** ONNX Runtime with the int8 dynamically quantised export of the *same*
  weights.

| | Throughput (6 threads) | Cosine vs original fp32 |
|---|---|---|
| candle fp32, 512 tokens | 0.8 chunks/s | 1.000 |
| candle fp32, fused softmax, 256 tokens | 3.6 chunks/s | 1.000 |
| ONNX Runtime fp32, 256 tokens | 11 chunks/s | 1.000 |
| **ONNX Runtime int8, 256 tokens** | **14 chunks/s** | **0.996–0.997** |

Query embedding went from ~60 ms to ~10 ms. Quality loss from int8 is negligible
for retrieval: rankings on our test queries were unchanged.

### 2. Only the start of each file is embedded

On a real home, 40% of all text chunks were CSV rows. Every chunk stays in
full-text (keyword) search; semantic vectors cover:
* data files (csv/tsv/json/jsonl/xml/log): the first chunk (header and first rows),
* everything else: the first 16 chunks, about 4 pages.

This roughly quarters the embedding work. Documents and notes are embedded
before code and data, so the most useful meaning search arrives first.

### 3. CLIP text encoder: activation bug fixed

candle 0.11's CLIP implementation hard-codes the QuickGELU activation in the text
encoder. LAION's OpenCLIP checkpoints use exact (erf) GELU. Cairn vendors the CLIP
module (`crates/cairn-ml/src/clip/`, MIT/Apache, candle authors) with:
* the activation taken from the config,
* the text and vision towers loadable separately. The daemon holds only the text
  tower for queries; the idle-priority worker holds only the vision tower.

### 4. Image pre-processing speed-ups

* JPEGs are decoded with DCT downscaling straight to ~224 px, 4–8x less work than
  a full decode of a 24 MP photo.
* EXIF orientation is applied, so rotated phone photos are understood correctly.
* Resize uses CatmullRom (≈ bicubic) and centre-crops to 224×224, with CLIP's
  mean/std normalisation.

### 5. Search uses compact vectors

Stored vectors are int8 with a per-vector scale (4x smaller than fp32). Search
works in two steps:
1. a brute-force scan over 1-bit sign codes held in RAM (48 bytes per text vector),
2. exact rescoring of the best candidates.

There's no HNSW graph to rebuild or corrupt.

### Score thresholds

Results below a similarity floor are dropped, so junk queries return nothing
instead of noise:

| Model | Floor | Must also be within |
|---|---|---|
| text | 0.60 | 0.10 of the best hit |
| CLIP | 0.22 | 0.06 of the best hit |

These were calibrated on our test corpus (`crates/cairnd/src/search.rs`).

## Files and hashes

```sh
cairn models list     # URLs, sizes and SHA-256 of every file
```

| Model directory | Files |
|---|---|
| `bge-small-en-v1.5/` | `tokenizer.json`, `model_int8.onnx` |
| `clip-vit-b32-laion2b/` | `config.json`, `tokenizer.json`, `model.safetensors` |

Default location: `~/.local/share/cairn/models/`.

## Changing models

A different text model changes `TEXT_MODEL_TAG`. The daemon then discards the old
text vectors and re-embeds in the background, so vectors from different models
are never mixed. See [DECISIONS.md](DECISIONS.md) D-012, D-013, D-015, D-061 and
D-062.
