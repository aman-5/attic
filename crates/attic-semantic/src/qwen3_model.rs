//! Native Candle transformer model for `Qwen3-Embedding-0.6B` (CP7 / F6).
//!
//! Provides the exact forward pass for Qwen3 transformer architecture:
//! - RoPE with `head_dim = 128` and `rope_theta = 1000000.0`
//! - Per-head query and key RMSNorm (`q_norm`, `k_norm`)
//! - Grouped Query Attention (GQA: 16 query heads, 8 KV heads)
//! - Combined causal + padding attention mask
//! - SwiGLU MLP activation
//! - Completely stateless forward pass (no KV cache accumulation during embedding)

use candle_core::{DType, Device, Module, Result, Tensor};
use candle_nn::VarBuilder;
use candle_nn::attention::{AttnMask, flash_attn};
use candle_transformers::models::with_tracing::{Linear, RmsNorm, linear_b, linear_no_bias};
use candle_transformers::utils::repeat_kv;
use std::sync::Arc;

fn default_rope_theta() -> f64 {
    1_000_000.0
}

fn default_rms_norm_eps() -> f64 {
    1e-6
}

/// Architecture configuration for Qwen3.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Qwen3Config {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub intermediate_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub head_dim: usize,
    #[serde(default)]
    pub attention_bias: bool,
    pub num_key_value_heads: usize,
    pub max_position_embeddings: usize,
    #[serde(default = "default_rope_theta")]
    pub rope_theta: f64,
    #[serde(default = "default_rms_norm_eps")]
    pub rms_norm_eps: f64,
}

#[derive(Debug, Clone)]
pub struct Qwen3RotaryEmbedding {
    sin: Tensor,
    cos: Tensor,
}

impl Qwen3RotaryEmbedding {
    pub fn new(dtype: DType, cfg: &Qwen3Config, dev: &Device) -> Result<Self> {
        let dim = cfg.head_dim;
        let max_seq_len = cfg.max_position_embeddings;
        let inv_freq: Vec<_> = (0..dim)
            .step_by(2)
            .map(|i| 1f32 / cfg.rope_theta.powf(i as f64 / dim as f64) as f32)
            .collect();
        let inv_freq_len = inv_freq.len();
        let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?.to_dtype(DType::F32)?;
        let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
            .to_dtype(DType::F32)?
            .reshape((max_seq_len, 1))?;
        let freqs = t.matmul(&inv_freq)?;
        Ok(Self {
            sin: freqs.sin()?.to_dtype(dtype)?,
            cos: freqs.cos()?.to_dtype(dtype)?,
        })
    }

    pub fn apply(&self, q: &Tensor, k: &Tensor) -> Result<(Tensor, Tensor)> {
        let (_, _, seq_len, _) = q.dims4()?;
        let cos = self.cos.narrow(0, 0, seq_len)?;
        let sin = self.sin.narrow(0, 0, seq_len)?;
        let q_embed = candle_nn::rotary_emb::rope(&q.contiguous()?, &cos, &sin)?;
        let k_embed = candle_nn::rotary_emb::rope(&k.contiguous()?, &cos, &sin)?;
        Ok((q_embed, k_embed))
    }
}

#[derive(Debug, Clone)]
pub struct Qwen3Attention {
    q_proj: Linear,
    k_proj: Linear,
    v_proj: Linear,
    o_proj: Linear,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    num_heads: usize,
    num_kv_heads: usize,
    num_kv_groups: usize,
    head_dim: usize,
    rotary_emb: Arc<Qwen3RotaryEmbedding>,
}

impl Qwen3Attention {
    pub fn new(
        cfg: &Qwen3Config,
        rotary_emb: Arc<Qwen3RotaryEmbedding>,
        vb: VarBuilder,
    ) -> Result<Self> {
        let head_dim = cfg.head_dim;
        let num_heads = cfg.num_attention_heads;
        let num_kv_heads = cfg.num_key_value_heads;
        let num_kv_groups = num_heads / num_kv_heads;

        let q_proj = linear_b(
            cfg.hidden_size,
            num_heads * head_dim,
            cfg.attention_bias,
            vb.pp("q_proj"),
        )?;
        let k_proj = linear_b(
            cfg.hidden_size,
            num_kv_heads * head_dim,
            cfg.attention_bias,
            vb.pp("k_proj"),
        )?;
        let v_proj = linear_b(
            cfg.hidden_size,
            num_kv_heads * head_dim,
            cfg.attention_bias,
            vb.pp("v_proj"),
        )?;
        let o_proj = linear_b(
            num_heads * head_dim,
            cfg.hidden_size,
            cfg.attention_bias,
            vb.pp("o_proj"),
        )?;

        let q_norm = RmsNorm::new(head_dim, cfg.rms_norm_eps, vb.pp("q_norm"))?;
        let k_norm = RmsNorm::new(head_dim, cfg.rms_norm_eps, vb.pp("k_norm"))?;

        Ok(Self {
            q_proj,
            k_proj,
            v_proj,
            o_proj,
            q_norm,
            k_norm,
            num_heads,
            num_kv_heads,
            num_kv_groups,
            head_dim,
            rotary_emb,
        })
    }

    pub fn forward(&self, x: &Tensor, attn_mask: Option<&Tensor>) -> Result<Tensor> {
        let (b, l, _) = x.dims3()?;

        let q = self.q_proj.forward(x)?;
        let k = self.k_proj.forward(x)?;
        let v = self.v_proj.forward(x)?;

        let q = q.reshape((b, l, self.num_heads, self.head_dim))?;
        let k = k.reshape((b, l, self.num_kv_heads, self.head_dim))?;
        let v = v
            .reshape((b, l, self.num_kv_heads, self.head_dim))?
            .transpose(1, 2)?;

        // Per-head RMSNorm directly on contiguous (b, l, heads, head_dim) before transpose
        let q = self.q_norm.forward(&q)?;
        let k = self.k_norm.forward(&k)?;

        let q = q.transpose(1, 2)?;
        let k = k.transpose(1, 2)?;

        // RoPE
        let (q, k) = self.rotary_emb.apply(&q, &k)?;

        // Fused CPU Flash Attention path (Candle 0.11.0)
        // Eliminates repeat_kv allocation, full QxK^T score materialization, softmax buffer allocations,
        // and separate V matmul by executing candle-nn's optimized CPU flash_attn kernel.
        if x.device().is_cpu() {
            let q_flash = q.transpose(1, 2)?.contiguous()?; // (B, L, H, D)
            let k_flash = k.transpose(1, 2)?.contiguous()?; // (B, L, KV_H, D)
            let v_flash = v.transpose(1, 2)?.contiguous()?; // (B, L, KV_H, D)

            let scale = 1.0 / (self.head_dim as f32).sqrt();

            let flash_mask = match attn_mask {
                Some(m) if b == 1 => AttnMask::Mask(m.clone()),
                None => AttnMask::causal_with_offset(0),
                _ => AttnMask::None,
            };

            if let Ok(ctx) =
                flash_attn::<f32>(&q_flash, &k_flash, &v_flash, scale, flash_mask, None, None)
            {
                // Output from CPU flash attention is (B, H, S, D), transpose to (B, S, H, D)
                return ctx
                    .transpose(1, 2)?
                    .reshape((b, l, self.num_heads * self.head_dim))?
                    .apply(&self.o_proj);
            }
        }

        // Standard fallback path for non-CPU devices or unsupported mask configurations
        // GQA repeat_kv
        let k = repeat_kv(k, self.num_kv_groups)?.contiguous()?;
        let v = repeat_kv(v, self.num_kv_groups)?.contiguous()?;

        let scale = 1.0 / (self.head_dim as f64).sqrt();
        let q = (q * scale)?;
        let mut scores = q.matmul(&k.transpose(2, 3)?)?;
        if let Some(mask) = attn_mask {
            scores = scores.broadcast_add(mask)?;
        }
        let probs = candle_nn::ops::softmax_last_dim(&scores)?;
        let ctx = probs.matmul(&v)?; // (B, H, L, D)

        ctx.transpose(1, 2)?
            .reshape((b, l, self.num_heads * self.head_dim))?
            .apply(&self.o_proj)
    }
}

#[derive(Debug, Clone)]
pub struct Qwen3MLP {
    gate_proj: Linear,
    up_proj: Linear,
    down_proj: Linear,
}

impl Qwen3MLP {
    pub fn new(cfg: &Qwen3Config, vb: VarBuilder) -> Result<Self> {
        Ok(Self {
            gate_proj: linear_no_bias(cfg.hidden_size, cfg.intermediate_size, vb.pp("gate_proj"))?,
            up_proj: linear_no_bias(cfg.hidden_size, cfg.intermediate_size, vb.pp("up_proj"))?,
            down_proj: linear_no_bias(cfg.intermediate_size, cfg.hidden_size, vb.pp("down_proj"))?,
        })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let gate = candle_nn::ops::silu(&x.apply(&self.gate_proj)?)?;
        let up = x.apply(&self.up_proj)?;
        (gate * up)?.apply(&self.down_proj)
    }
}

#[derive(Debug, Clone)]
pub struct DecoderLayer {
    self_attn: Qwen3Attention,
    mlp: Qwen3MLP,
    input_layernorm: RmsNorm,
    post_attention_layernorm: RmsNorm,
}

impl DecoderLayer {
    pub fn new(
        cfg: &Qwen3Config,
        rotary: Arc<Qwen3RotaryEmbedding>,
        vb: VarBuilder,
    ) -> Result<Self> {
        let self_attn = Qwen3Attention::new(cfg, rotary, vb.pp("self_attn"))?;
        let mlp = Qwen3MLP::new(cfg, vb.pp("mlp"))?;
        let input_layernorm =
            RmsNorm::new(cfg.hidden_size, cfg.rms_norm_eps, vb.pp("input_layernorm"))?;
        let post_attention_layernorm = RmsNorm::new(
            cfg.hidden_size,
            cfg.rms_norm_eps,
            vb.pp("post_attention_layernorm"),
        )?;
        Ok(Self {
            self_attn,
            mlp,
            input_layernorm,
            post_attention_layernorm,
        })
    }

    pub fn forward(&self, x: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        let residual = x;
        let h = self.input_layernorm.forward(x)?;
        let h = self.self_attn.forward(&h, mask)?;
        let x = (residual + h)?;
        let residual = &x;
        let h = self.post_attention_layernorm.forward(&x)?;
        let h = self.mlp.forward(&h)?;
        residual + h
    }
}

#[derive(Debug, Clone)]
pub struct Qwen3Model {
    embed_tokens: candle_nn::Embedding,
    layers: Vec<DecoderLayer>,
    norm: RmsNorm,
}

impl Qwen3Model {
    pub fn new(cfg: &Qwen3Config, vb: VarBuilder) -> Result<Self> {
        // Handle both standard Hugging Face causal LM layout ("model.embed_tokens")
        // and base embedding layout ("embed_tokens")
        let vb_root = if vb.contains_tensor("embed_tokens.weight") {
            vb.clone()
        } else if vb.contains_tensor("model.embed_tokens.weight") {
            vb.pp("model")
        } else {
            vb.clone()
        };

        let embed_tokens =
            candle_nn::embedding(cfg.vocab_size, cfg.hidden_size, vb_root.pp("embed_tokens"))?;
        let rotary = Arc::new(Qwen3RotaryEmbedding::new(
            DType::F32,
            cfg,
            vb_root.device(),
        )?);
        let vb_l = vb_root.pp("layers");
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            layers.push(DecoderLayer::new(cfg, rotary.clone(), vb_l.pp(i))?);
        }
        let norm = RmsNorm::new(cfg.hidden_size, cfg.rms_norm_eps, vb_root.pp("norm"))?;
        Ok(Self {
            embed_tokens,
            layers,
            norm,
        })
    }

    /// Build combined causal + padding attention mask.
    /// Shape: (B, 1, L, L)
    /// `mask[b, 0, i, j] = 0.0` if `(j <= i && attention_mask[b, j] == 1)` else `-10000.0`.
    pub fn build_attention_mask(
        attention_mask_rows: &[Vec<u32>],
        seq_len: usize,
        device: &Device,
    ) -> Result<Tensor> {
        let b_sz = attention_mask_rows.len();
        let all_active_global = attention_mask_rows
            .iter()
            .all(|row| row.len() >= seq_len && row[..seq_len].iter().all(|&v| v > 0));

        if all_active_global {
            let mut mask_data = Vec::with_capacity(seq_len * seq_len);
            for i in 0..seq_len {
                for j in 0..seq_len {
                    mask_data.push(if j <= i { 0.0f32 } else { -1e4f32 });
                }
            }
            Tensor::from_vec(mask_data, (1, 1, seq_len, seq_len), device)
        } else {
            let mut mask_data = Vec::with_capacity(b_sz * seq_len * seq_len);
            for row in attention_mask_rows {
                let row_all_active = row.len() >= seq_len && row[..seq_len].iter().all(|&v| v > 0);
                if row_all_active {
                    for i in 0..seq_len {
                        for j in 0..seq_len {
                            mask_data.push(if j <= i { 0.0f32 } else { -1e4f32 });
                        }
                    }
                } else {
                    for i in 0..seq_len {
                        for j in 0..seq_len {
                            let is_active = row.get(j).copied().unwrap_or(0) > 0;
                            let is_causal = j <= i;
                            mask_data.push(if is_active && is_causal {
                                0.0f32
                            } else {
                                -1e4f32
                            });
                        }
                    }
                }
            }
            Tensor::from_vec(mask_data, (b_sz, 1, seq_len, seq_len), device)
        }
    }

    pub fn forward(&self, input_ids: &Tensor, attention_mask: Option<&Tensor>) -> Result<Tensor> {
        let mut h = self.embed_tokens.forward(input_ids)?;
        for layer in &self.layers {
            h = layer.forward(&h, attention_mask)?;
        }
        self.norm.forward(&h)
    }
}
