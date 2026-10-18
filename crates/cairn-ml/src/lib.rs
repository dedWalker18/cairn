//! Local embedding models. No network access: weights are installed once
//! (`cairn models fetch`, or copied in for airgapped machines) and verified
//! against SHA-256 hashes pinned in this file before they are ever loaded.

pub mod clip;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::VarBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// Download source when it differs from the spec: (repo, revision, path).
    pub src: Option<(&'static str, &'static str, &'static str)>,
}

pub struct ModelSpec {
    pub id: &'static str,
    pub repo: &'static str,
    pub revision: &'static str,
    pub license: &'static str,
    pub files: &'static [ModelFile],
}

impl ModelSpec {
    pub fn url(&self, file: &str) -> String {
        if let Some((repo, rev, path)) = self.files.iter().find(|f| f.name == file).and_then(|f| f.src) {
            return format!("https://huggingface.co/{repo}/resolve/{rev}/{path}");
        }
        format!("https://huggingface.co/{}/resolve/{}/{}", self.repo, self.revision, file)
    }
}

pub const TEXT_MODEL: ModelSpec = ModelSpec {
    id: "bge-small-en-v1.5",
    repo: "BAAI/bge-small-en-v1.5",
    revision: "5c38ec7c405ec4b44b94cc5a9bb96e735b38267a",
    license: "MIT",
    files: &[
        ModelFile { name: "tokenizer.json", size: 711396, sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66", src: None },
        // int8 dynamic quantisation of the same weights (transformers.js
        // export); cosine >= 0.996 vs fp32, ~4x faster on CPU (D-061).
        ModelFile {
            name: "model_int8.onnx",
            size: 34014426,
            sha256: "6c9c6101a956d62dfb5e7190c538226c0c5bb9cb27b651234b6df063ee7dbfe4",
            src: Some(("Xenova/bge-small-en-v1.5", "ea104dacec62c0de699686887e3f920caeb4f3e3", "onnx/model_quantized.onnx")),
        },
    ],
};

pub const CLIP_MODEL: ModelSpec = ModelSpec {
    id: "clip-vit-b32-laion2b",
    repo: "laion/CLIP-ViT-B-32-laion2B-s34B-b79K",
    revision: "1a25a446712ba5ee05982a381eed697ef9b435cf",
    license: "MIT",
    files: &[
        ModelFile { name: "config.json", size: 4355, sha256: "1284cbff35169abb23a1c5525a8b0f543c7bd191d4b9aed63880c1571bc4191c", src: None },
        ModelFile { name: "tokenizer.json", size: 2224041, sha256: "b556ac8c99757ffb677208af34bc8c6721572114111a6e0aaf5fa69ff0b8d842", src: None },
        ModelFile { name: "model.safetensors", size: 605157884, sha256: "74813fbcdc750f235c9784c367ca1394d2a5c25eb0aac92761752ac239db7cff", src: None },
    ],
};

pub const ALL_MODELS: [&ModelSpec; 2] = [&TEXT_MODEL, &CLIP_MODEL];

pub const TEXT_DIM: usize = 384;
pub const CLIP_DIM: usize = 512;
pub const CLIP_IMAGE: usize = 224;
const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
const CLIP_MEAN: [f32; 3] = [0.48145466, 0.4578275, 0.40821073];
const CLIP_STD: [f32; 3] = [0.26862954, 0.26130258, 0.27577711];

pub fn model_dir(models_root: &Path, spec: &ModelSpec) -> PathBuf {
    models_root.join(spec.id)
}

#[derive(Serialize, Deserialize, Default)]
struct VerifiedCache {
    entries: Vec<(String, u64, i128, String)>,
}

pub fn sha256_file(p: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Check every file's size and SHA-256. Hashing 600 MB takes a second or two,
/// so results are cached by (size, mtime) in `.verified`; the cache only
/// skips re-hashing files whose metadata is unchanged.
pub fn verify(models_root: &Path, spec: &ModelSpec) -> Result<()> {
    let dir = model_dir(models_root, spec);
    let cache_path = dir.join(".verified");
    let cache: VerifiedCache = std::fs::read(&cache_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut fresh = VerifiedCache::default();
    for f in spec.files {
        let p = dir.join(f.name);
        let meta = std::fs::metadata(&p).with_context(|| format!("model file missing: {}", p.display()))?;
        if meta.len() != f.size {
            bail!("{}: size {} != expected {}", p.display(), meta.len(), f.size);
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i128).unwrap_or(0);
        let cached = cache.entries.iter().any(|(n, s, m, h)| n == f.name && *s == f.size && *m == mtime && h == f.sha256);
        if !cached {
            let h = sha256_file(&p)?;
            if h != f.sha256 {
                bail!("{}: sha256 mismatch (got {h})", p.display());
            }
        }
        fresh.entries.push((f.name.to_string(), f.size, mtime, f.sha256.to_string()));
    }
    if let Ok(b) = serde_json::to_vec(&fresh) {
        let _ = std::fs::write(&cache_path, b);
    }
    Ok(())
}

fn tokenizer(path: &Path, max_len: usize, pad: bool) -> Result<Tokenizer> {
    let mut tok = Tokenizer::from_file(path).map_err(anyhow::Error::msg)?;
    tok.with_truncation(Some(TruncationParams { max_length: max_len, ..Default::default() })).map_err(anyhow::Error::msg)?;
    if pad {
        tok.with_padding(Some(PaddingParams { strategy: PaddingStrategy::BatchLongest, ..Default::default() }));
    } else {
        tok.with_padding(None);
    }
    Ok(tok)
}

unsafe fn var_builder(file: &Path) -> Result<VarBuilder<'static>> {
    // Model files are ours, verified and never modified in place, so mapping
    // them is safe (the no-mmap rule is for files that can be truncated).
    Ok(VarBuilder::from_mmaped_safetensors(&[file.to_path_buf()], DType::F32, &Device::Cpu)?)
}

/// Identifies the text embedding model; a change re-embeds the text space.
pub const TEXT_MODEL_TAG: &str = "bge-small-en-v1.5/int8-onnx/256";

/// bge-small-en-v1.5 via ONNX Runtime (int8). CLS pooling, normalised.
pub struct TextEmbedder {
    session: std::sync::Mutex<ort::session::Session>,
    tok: Tokenizer,
}

fn ort_err(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("onnxruntime: {e}")
}

impl TextEmbedder {
    pub fn load(models_root: &Path) -> Result<TextEmbedder> {
        Self::load_with_threads(models_root, 1)
    }

    /// `threads`: intra-op threads (1 for latency-sensitive queries, more
    /// for bulk indexing in the idle-priority worker).
    pub fn load_with_threads(models_root: &Path, threads: usize) -> Result<TextEmbedder> {
        verify(models_root, &TEXT_MODEL)?;
        let dir = model_dir(models_root, &TEXT_MODEL);
        // 256 tokens: chunks are ~1000 characters; attention cost is quadratic.
        let tok = tokenizer(&dir.join("tokenizer.json"), 256, true)?;
        let env = ort::init().with_name("cairn").with_telemetry(false).build().map_err(ort_err)?;
        let session = ort::session::Session::builder(&env)
            .map_err(ort_err)?
            .with_intra_threads(threads.max(1))
            .map_err(ort_err)?
            .with_inter_threads(1)
            .map_err(ort_err)?
            .commit_from_file(dir.join("model_int8.onnx"))
            .map_err(ort_err)?;
        Ok(TextEmbedder { session: std::sync::Mutex::new(session), tok })
    }

    /// Embed passages (documents, chunks, folder context).
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        use ort::value::Tensor as OrtTensor;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let enc = self.tok.encode_batch(texts.to_vec(), true).map_err(anyhow::Error::msg)?;
        let (b, n) = (enc.len(), enc[0].get_ids().len());
        let ids: Vec<i64> = enc.iter().flat_map(|e| e.get_ids().iter().map(|x| *x as i64)).collect();
        let mask: Vec<i64> = enc.iter().flat_map(|e| e.get_attention_mask().iter().map(|x| *x as i64)).collect();
        let types = vec![0i64; b * n];
        let mut session = self.session.lock().map_err(|_| anyhow::anyhow!("embedder poisoned"))?;
        let out = session
            .run(ort::inputs![
                "input_ids" => OrtTensor::from_array(([b, n], ids.into_boxed_slice())).map_err(ort_err)?,
                "attention_mask" => OrtTensor::from_array(([b, n], mask.into_boxed_slice())).map_err(ort_err)?,
                "token_type_ids" => OrtTensor::from_array(([b, n], types.into_boxed_slice())).map_err(ort_err)?,
            ])
            .map_err(ort_err)?;
        let (shape, data) = out[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        let dim = *shape.last().context("bad output shape")? as usize;
        if dim != TEXT_DIM || data.len() != b * n * dim {
            bail!("unexpected embedding output shape {shape:?}");
        }
        Ok((0..b)
            .map(|i| {
                let mut v = data[i * n * dim..i * n * dim + dim].to_vec();
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 && norm.is_finite() {
                    v.iter_mut().for_each(|x| *x /= norm);
                }
                v
            })
            .collect())
    }

    pub fn embed_query(&self, q: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[format!("{BGE_QUERY_PREFIX}{q}")])?;
        v.pop().context("no embedding")
    }
}

fn clip_text_config() -> clip::text_model::ClipTextConfig {
    let mut c = clip::text_model::ClipTextConfig::vit_base_patch32();
    c.activation = clip::text_model::Activation::Gelu;
    c
}

fn clip_vision_config() -> clip::vision_model::ClipVisionConfig {
    let mut c = clip::vision_model::ClipVisionConfig::vit_base_patch32();
    c.activation = clip::text_model::Activation::Gelu;
    c
}

/// CLIP text tower: turns a query into the image embedding space.
pub struct ClipText {
    model: clip::text_model::ClipTextTransformer,
    proj: candle_nn::Linear,
    tok: Tokenizer,
}

impl ClipText {
    pub fn load(models_root: &Path) -> Result<ClipText> {
        verify(models_root, &CLIP_MODEL)?;
        let dir = model_dir(models_root, &CLIP_MODEL);
        let vb = unsafe { var_builder(&dir.join("model.safetensors"))? };
        let cfg = clip_text_config();
        let model = clip::text_model::ClipTextTransformer::new(vb.pp("text_model"), &cfg)?;
        let proj = candle_nn::linear_no_bias(cfg.embed_dim, cfg.projection_dim, vb.pp("text_projection"))?;
        let tok = tokenizer(&dir.join("tokenizer.json"), 77, false)?;
        Ok(ClipText { model, proj, tok })
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let enc = self.tok.encode(text, true).map_err(anyhow::Error::msg)?;
        let ids = Tensor::new(enc.get_ids(), &Device::Cpu)?.unsqueeze(0)?;
        let f = self.model.forward(&ids)?.apply(&self.proj)?;
        let f = clip::div_l2_norm(&f)?;
        Ok(f.to_vec2::<f32>()?.pop().context("no embedding")?)
    }
}

/// CLIP vision tower.
pub struct ClipVision {
    model: clip::vision_model::ClipVisionTransformer,
    proj: candle_nn::Linear,
}

impl ClipVision {
    pub fn load(models_root: &Path) -> Result<ClipVision> {
        verify(models_root, &CLIP_MODEL)?;
        let dir = model_dir(models_root, &CLIP_MODEL);
        let vb = unsafe { var_builder(&dir.join("model.safetensors"))? };
        let cfg = clip_vision_config();
        let model = clip::vision_model::ClipVisionTransformer::new(vb.pp("vision_model"), &cfg)?;
        let proj = candle_nn::linear_no_bias(cfg.embed_dim, cfg.projection_dim, vb.pp("visual_projection"))?;
        Ok(ClipVision { model, proj })
    }

    /// `images`: RGB u8, CLIP_IMAGE x CLIP_IMAGE each.
    pub fn embed(&self, images: &[Vec<u8>]) -> Result<Vec<Vec<f32>>> {
        let n = CLIP_IMAGE * CLIP_IMAGE;
        let mut data = Vec::with_capacity(images.len() * 3 * n);
        for img in images {
            if img.len() != 3 * n {
                bail!("bad image buffer size {}", img.len());
            }
            for c in 0..3 {
                for i in 0..n {
                    data.push((img[i * 3 + c] as f32 / 255.0 - CLIP_MEAN[c]) / CLIP_STD[c]);
                }
            }
        }
        let t = Tensor::from_vec(data, (images.len(), 3, CLIP_IMAGE, CLIP_IMAGE), &Device::Cpu)?;
        let f = self.model.forward(&t)?.apply(&self.proj)?;
        let f = clip::div_l2_norm(&f)?;
        Ok(f.to_vec2::<f32>()?)
    }
}
