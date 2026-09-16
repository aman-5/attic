//! Versioned, length-prefixed IPC protocol between the Attic server
//! (supervisor) and the isolated inference worker process (r06).
//!
//! Wire format: 4-byte little-endian length prefix + one UTF-8 JSON message.
//! Rationale for a child process at all: neural runtimes (DirectML, CUDA,
//! Candle) can hang or corrupt state inside a blocking `run()` call that no
//! in-process timeout can interrupt. In a separate process the supervisor
//! can `kill()` a stuck worker, expire its leases, and restart clean — the
//! MCP server stays alive and responsive throughout.
//!
//! Protocol rules:
//! * `PROTOCOL_VERSION` must match exactly; a mismatch is a permanent error.
//! * Frames larger than `MAX_FRAME_BYTES` are rejected before allocation.
//! * Every request carries a monotonically increasing `id`; responses echo
//!   it. After a worker restart the supervisor's id space continues, so a
//!   stale worker could never impersonate a new one even in principle.
#![forbid(unsafe_code)]
#![deny(clippy::all)]

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub mod engine;
pub mod supervisor;

pub const PROTOCOL_VERSION: u32 = 1;
/// Hard frame cap — a batch of embedding texts is bounded well below this;
/// anything larger indicates corruption or a hostile peer.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerRequest {
    /// First message on worker start; worker replies with `HelloResponse`.
    Hello { id: u64 },
    /// Load the model. Worker replies Ok or Error(artifact/...).
    LoadModel {
        id: u64,
        cache_dir: String,
        batch_size: usize,
        dimension: Option<usize>,
        /// Backend selector: "candle-cpu" | "ort-directml" (worker compiled
        /// with the feature; absence is an artifact-class error).
        backend: String,
        /// Directory containing model_fp16.onnx + tokenizer.json (DirectML).
        onnx_model_dir: Option<String>,
        /// Fixed padded sequence length (DirectML static shape).
        seq_len: Option<usize>,
    },
    /// Embed a batch; `deadline_ms` is the worker-side wall-clock budget.
    EmbedBatch {
        id: u64,
        items: Vec<EmbedItem>,
        deadline_ms: u64,
    },
    /// Graceful shutdown; worker replies Ok then exits.
    Shutdown { id: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedItem {
    pub key: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerResponse {
    HelloOk {
        id: u64,
        protocol: u32,
        backend: String,
        capabilities: Vec<String>,
    },
    ModelReady {
        id: u64,
        dimension: usize,
        max_input_bytes: usize,
        backend: String,
    },
    Embeddings {
        id: u64,
        /// One vector per request item, in request order.
        vectors: Vec<Vec<f32>>,
        items_embedded: u64,
        elapsed_ms: u64,
    },
    Ok {
        id: u64,
    },
    Error {
        id: u64,
        class: WorkerErrorClass,
        message: String,
    },
}

/// Typed failure classes driving the supervisor's retry/kill decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerErrorClass {
    /// GPU/VRAM or native allocation failure — retry with smaller batch.
    OutOfMemory,
    /// Worker-side deadline exceeded (it cooperatively aborted).
    Timeout,
    /// Over-token or otherwise permanently invalid input.
    InvalidInput,
    /// Model artifact missing/corrupt/unsupported backend.
    Artifact,
    /// Anything else.
    Internal,
}

// ── Framing ────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("I/O error on worker channel: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {0} bytes (max {MAX_FRAME_BYTES})")]
    FrameTooLarge(u32),
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("protocol version mismatch: worker speaks {worker}, supervisor speaks {supervisor}")]
    VersionMismatch { worker: u32, supervisor: u32 },
    #[error("worker closed the channel unexpectedly")]
    UnexpectedEof,
}

fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<(), ProtocolError> {
    let payload = serde_json::to_vec(msg).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    if payload.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(ProtocolError::FrameTooLarge(payload.len() as u32));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&payload)?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> Result<Option<T>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ProtocolError::Io(e)),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    // Strict pre-allocation bound: the cap check above runs BEFORE alloc.
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            ProtocolError::UnexpectedEof
        } else {
            ProtocolError::Io(e)
        }
    })?;
    let msg = serde_json::from_slice(&buf).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    Ok(Some(msg))
}

pub fn write_request(w: &mut impl Write, req: &WorkerRequest) -> Result<(), ProtocolError> {
    write_frame(w, req)
}

pub fn read_request(r: &mut impl Read) -> Result<Option<WorkerRequest>, ProtocolError> {
    read_frame(r)
}

pub fn write_response(w: &mut impl Write, resp: &WorkerResponse) -> Result<(), ProtocolError> {
    write_frame(w, resp)
}

pub fn read_response(r: &mut impl Read) -> Result<Option<WorkerResponse>, ProtocolError> {
    read_frame(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_response_roundtrip() {
        let mut buf = Vec::new();
        let req = WorkerRequest::EmbedBatch {
            id: 7,
            items: vec![EmbedItem {
                key: "k".into(),
                text: "hello".into(),
            }],
            deadline_ms: 1000,
        };
        write_request(&mut buf, &req).unwrap();
        let mut slice: &[u8] = &buf;
        let got = read_request(&mut slice).unwrap().unwrap();
        assert_eq!(got, req);
    }

    #[test]
    fn oversized_frame_rejected_before_allocation() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        let mut slice: &[u8] = &buf;
        let err = read_request(&mut slice).unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
    }

    #[test]
    fn truncated_frame_is_unexpected_eof() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"short");
        let mut slice: &[u8] = &buf;
        let err = read_request(&mut slice).unwrap_err();
        assert!(matches!(err, ProtocolError::UnexpectedEof));
    }

    #[test]
    fn clean_eof_is_none() {
        let mut slice: &[u8] = &[];
        assert!(read_request(&mut slice).unwrap().is_none());
    }
}
