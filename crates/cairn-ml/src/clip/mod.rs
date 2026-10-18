//! CLIP text and vision towers, vendored from candle-transformers 0.11
//! (MIT OR Apache-2.0, Copyright the candle authors).
//!
//! Cairn changes: the text encoder honours the configured activation
//! (upstream hard-codes QuickGELU, which is wrong for OpenCLIP/LAION weights
//! that use exact GELU), and the towers load independently so the daemon
//! holds only the text tower and the embed worker only the vision tower.

pub mod text_model;
pub mod vision_model;

use candle_core::{Result, Tensor, D};
use text_model::Activation;

#[derive(Clone, Debug)]
pub enum EncoderConfig {
    Text(text_model::ClipTextConfig),
    Vision(vision_model::ClipVisionConfig),
}

impl EncoderConfig {
    pub fn embed_dim(&self) -> usize {
        match self {
            Self::Text(c) => c.embed_dim,
            Self::Vision(c) => c.embed_dim,
        }
    }

    pub fn num_attention_heads(&self) -> usize {
        match self {
            Self::Text(c) => c.num_attention_heads,
            Self::Vision(c) => c.num_attention_heads,
        }
    }

    pub fn intermediate_size(&self) -> usize {
        match self {
            Self::Text(c) => c.intermediate_size,
            Self::Vision(c) => c.intermediate_size,
        }
    }

    pub fn num_hidden_layers(&self) -> usize {
        match self {
            Self::Text(c) => c.num_hidden_layers,
            Self::Vision(c) => c.num_hidden_layers,
        }
    }

    pub fn activation(&self) -> Activation {
        match self {
            Self::Text(c) => c.activation,
            Self::Vision(c) => c.activation,
        }
    }
}

pub fn div_l2_norm(v: &Tensor) -> Result<Tensor> {
    let l2_norm = v.sqr()?.sum_keepdim(D::Minus1)?.sqrt()?;
    v.broadcast_div(&l2_norm)
}
