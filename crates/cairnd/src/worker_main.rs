//! Worker process entry points. Workers read frames on stdin and answer on
//! stdout. They are sandboxed before reading a single request:
//! no filesystem writes, no TCP, idle priority, capped memory, die with parent.

use std::io::{BufReader, BufWriter};

use anyhow::{bail, Result};
use cairn_core::extract::{self, ExtractRequest, ExtractResponse};
use cairn_core::{ipc, sandbox};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum EmbedRequest {
    Text { texts: Vec<String> },
    /// Payload: `count` RGB images of CLIP_IMAGE^2 * 3 bytes.
    Images { count: usize },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum EmbedResponse {
    /// Payload: `count * dim` little-endian f32.
    Vectors { count: usize, dim: usize },
    Error { message: String },
}

fn sandbox(mem: u64) -> Result<()> {
    sandbox::harden_worker(mem);
    sandbox::set_thread_idle();
    let dev_null = std::path::Path::new("/dev/null");
    let e = sandbox::restrict_writes(&[dev_null])?;
    if e == sandbox::Enforcement::Unsupported && std::env::var_os("CAIRN_ALLOW_NO_LANDLOCK").is_none() {
        bail!("Landlock is not available; refusing to parse files unsandboxed (set CAIRN_ALLOW_NO_LANDLOCK=1 to override)");
    }
    Ok(())
}

pub fn run(kind: &str) -> Result<()> {
    match kind {
        "extract" => {
            sandbox(1536 << 20)?;
            let mut input = BufReader::new(std::io::stdin().lock());
            let mut output = BufWriter::new(std::io::stdout().lock());
            while let Some((req, _)) = ipc::read_frame::<_, ExtractRequest>(&mut input)? {
                let (resp, payload) = std::panic::catch_unwind(|| extract::handle(&req))
                    .unwrap_or_else(|_| (ExtractResponse::Error { message: "extractor panicked".into() }, Vec::new()));
                ipc::write_frame(&mut output, &resp, &payload)?;
            }
            Ok(())
        }
        "embed" => {
            // Virtual address space: model mapping + runtime arenas + rayon stacks.
            sandbox(8 << 30)?;
            let models = cairn_core::paths::models_dir()?;
            let mut text: Option<cairn_ml::TextEmbedder> = None;
            let mut vision: Option<cairn_ml::ClipVision> = None;
            let mut input = BufReader::new(std::io::stdin().lock());
            let mut output = BufWriter::new(std::io::stdout().lock());
            while let Some((req, payload)) = ipc::read_frame::<_, EmbedRequest>(&mut input)? {
                let r: Result<(usize, usize, Vec<Vec<f32>>)> = (|| match req {
                    EmbedRequest::Text { texts } => {
                        if text.is_none() {
                            let threads = std::env::var("RAYON_NUM_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
                            text = Some(cairn_ml::TextEmbedder::load_with_threads(&models, threads)?);
                        }
                        let v = text.as_ref().unwrap().embed(&texts)?;
                        Ok((v.len(), cairn_ml::TEXT_DIM, v))
                    }
                    EmbedRequest::Images { count } => {
                        let sz = cairn_ml::CLIP_IMAGE * cairn_ml::CLIP_IMAGE * 3;
                        if payload.len() != count * sz {
                            bail!("image payload size mismatch");
                        }
                        if vision.is_none() {
                            vision = Some(cairn_ml::ClipVision::load(&models)?);
                        }
                        let imgs: Vec<Vec<u8>> = payload.chunks_exact(sz).map(|c| c.to_vec()).collect();
                        let v = vision.as_ref().unwrap().embed(&imgs)?;
                        Ok((v.len(), cairn_ml::CLIP_DIM, v))
                    }
                })();
                match r {
                    Ok((count, dim, vecs)) => {
                        let mut bytes = Vec::with_capacity(count * dim * 4);
                        for v in &vecs {
                            for x in v {
                                bytes.extend_from_slice(&x.to_le_bytes());
                            }
                        }
                        ipc::write_frame(&mut output, &EmbedResponse::Vectors { count, dim }, &bytes)?;
                    }
                    Err(e) => ipc::write_frame(&mut output, &EmbedResponse::Error { message: format!("{e:#}") }, &[])?,
                }
            }
            Ok(())
        }
        _ => bail!("unknown worker kind {kind:?}"),
    }
}
