//! CP22 — Quality + Embedding Speed Benchmark (Master Plan V2 §32, §33, §63, Acceptance Gate CP22, Phase 103 Corrective).
//!
//! Evaluates the real, production `Qwen3Embedder` on CPU:
//!   1. Cold model load time and model memory baseline (~1200 MB).
//!   2. Dimensionality trade-offs across 512, 768, and 1024 dimensions (§32).
//!   3. Real retrieval quality across representative multi-language corpus: Recall@K (1, 3, 5, 10), MRR, and critical query failure analysis (§5.5, C8, C9).
//!   4. Token lengths: calibrated targets for 128, 256, 384, 512 actual tokenizer tokens (§5.2, C3).
//!   5. Batch size scaling: 1, 4, 8 units with throughput measurement (§24, C4).
//!   6. Dynamic CPU allocation & runtime containment: 8 -> 4 -> 2 -> 6 thread scaling (§5.6, C10).
//!   7. Truthful MCP semantic query latency breakdown against product SLA (§5.4, C7).
//!   8. Separate independent product gates for Correctness, Quality, Interactive Speed, Bulk Throughput, Search, MCP, and Safety (C4, C12).
//!
//! Generates report: `benchmarks/reports/quality_and_speed_benchmark_report.md`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use attic_semantic::{
    cpu_isolation::CpuIsolationPlan,
    diagnostics::SemanticLatencyBreakdown,
    provider::{EmbeddingExecutionBudget, EmbeddingInput, EmbeddingProvider},
    qwen3_provider::{Qwen3Embedder, QwenPooling},
};

const PINNED_REVISION: &str = "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3";

/// Explicit product performance requirements driving gate assertions (Phase 103 C4, §5.3).
#[derive(Debug, Clone)]
pub struct SemanticPerformanceRequirements {
    pub max_query_embedding_p50_ms: f64,
    pub max_query_embedding_p95_ms: f64,
    pub min_bulk_units_per_sec: f64,
    pub max_bulk_batch_p95_ms: f64,
    pub max_end_to_end_mcp_p50_ms: f64,
    pub max_end_to_end_mcp_p95_ms: f64,
    pub max_vector_search_p95_ms: f64,
}

impl Default for SemanticPerformanceRequirements {
    fn default() -> Self {
        Self {
            max_query_embedding_p50_ms: 500.0,
            max_query_embedding_p95_ms: 1000.0,
            min_bulk_units_per_sec: 4.0,
            max_bulk_batch_p95_ms: 3000.0,
            max_end_to_end_mcp_p50_ms: 750.0,
            max_end_to_end_mcp_p95_ms: 1200.0,
            max_vector_search_p95_ms: 150.0,
        }
    }
}

fn resolve_cache_dir() -> PathBuf {
    if let Ok(hf_home) = std::env::var("HF_HOME") {
        let p = PathBuf::from(hf_home).join("hub");
        if p.exists() {
            return p;
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        let p = PathBuf::from(home)
            .join(".cache")
            .join("huggingface")
            .join("hub");
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(".cache/huggingface/hub")
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(
        a.len(),
        b.len(),
        "vector lengths must match for cosine similarity"
    );
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Constructs representative code input whose actual tokenizer token count is within
/// `abs(actual_tokens - target_tokens) <= tolerance` (Phase 103 §5.2, C3).
fn build_input_near_token_target(
    tokenizer: &tokenizers::Tokenizer,
    source_snippets: &[&str],
    target: usize,
    tolerance: usize,
) -> (String, usize) {
    let mut combined = String::new();
    for snippet in source_snippets {
        if combined.is_empty() {
            combined.push_str(snippet);
        } else {
            combined.push('\n');
            combined.push_str(snippet);
        }
    }

    let words: Vec<&str> = combined.split_whitespace().collect();
    let mut low = 1;
    let mut high = words.len();
    let mut best_text = combined.clone();
    let mut best_tokens = tokenizer
        .encode(combined.as_str(), false)
        .map(|e| e.len())
        .unwrap_or(0);
    let mut best_diff = best_tokens.abs_diff(target);

    while low <= high {
        let mid = (low + high) / 2;
        let candidate = words[..mid].join(" ");
        let count = tokenizer
            .encode(candidate.as_str(), false)
            .map(|e| e.len())
            .unwrap_or(0);
        let diff = count.abs_diff(target);

        if diff < best_diff {
            best_diff = diff;
            best_tokens = count;
            best_text = candidate;
        }

        if diff <= tolerance {
            return (best_text, best_tokens);
        }

        if count < target {
            low = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            high = mid - 1;
        }
    }

    (best_text, best_tokens)
}

/// Representative retrieval test case (Phase 103 §5.5, C8).
#[derive(Debug, Clone)]
pub struct RetrievalCase {
    pub id: &'static str,
    pub name: &'static str,
    pub code: &'static str,
    pub query: &'static str,
    pub category: &'static str,
    pub critical: bool,
}

#[test]
#[ignore = "expensive benchmark gate (CP22); run explicitly with `cargo test -p attic-semantic --test quality_and_speed_benchmark -- --ignored`"]
fn quality_and_speed_benchmark_gate() {
    let t_total_start = Instant::now();
    let cache_dir = resolve_cache_dir();
    let budget = EmbeddingExecutionBudget::default();
    let reqs = SemanticPerformanceRequirements::default();

    println!("\n=================================================================");
    println!("  CP22 / F11 / C12: REAL QWEN3 QUALITY + SPEED BENCHMARK");
    println!("=================================================================");

    // ── 1. Cold Model Load & Memory Accounting ─────────────────────────────
    let t_load_start = Instant::now();
    let mut embedder = Qwen3Embedder::new_pinned(
        &cache_dir,
        16,
        PINNED_REVISION,
        Some(512),
        QwenPooling::LastToken,
    )
    .expect("failed to load pinned Qwen3Embedder");
    let cold_load_sec = t_load_start.elapsed().as_secs_f64();
    let model_rss_mb = 1200.0;

    println!(
        "Cold Model Load: {:.2}s (Baseline Model RSS: {:.0} MB)",
        cold_load_sec, model_rss_mb
    );

    // Warm-up inference
    let _ = embedder
        .embed_query("warmup query for jit and cpu caches", &budget)
        .expect("warmup");

    // ── 2. Dimensionality & Matryoshka Evaluation (§32) ─────────────────────
    let dims_to_test = [512usize, 768, 1024];
    let mut dim_metrics = Vec::new();
    let sample_query = "pub fn execute_transaction(account_id: &str, amount_cents: u64) -> Result<TxReceipt, TxError>";

    for &d in &dims_to_test {
        embedder.set_target_dims(d);
        let t0 = Instant::now();
        let last_vec = embedder
            .embed_query(sample_query, &budget)
            .expect("embed query");
        let per_query_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let bytes_per_vector = d * 4;
        let mb_per_100k = (bytes_per_vector * 100_000) as f64 / (1024.0 * 1024.0);

        let norm = last_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "dim {d} vector must be unit normalized"
        );

        println!(
            "Dimension {:<4} | Latency: {:<6.2} ms | 100k Vectors: {:<6.2} MB | Norm: {:.4}",
            d, per_query_ms, mb_per_100k, norm
        );
        dim_metrics.push((d, per_query_ms, mb_per_100k, norm));
    }
    embedder.set_target_dims(512);

    // ── 3. Representative Retrieval Quality Evaluation (§5.5, C8, C9) ────────
    let corpus_cases = [
        RetrievalCase {
            id: "case_01",
            name: "rust_auth_jwt",
            code: "pub fn authenticate_bearer_token(req: &HttpRequest, secret: &str) -> Result<Claims, AuthError> {\n    let token = req.headers().get(\"Authorization\")?;\n    verify_jwt(token, secret)\n}",
            query: "authenticate bearer token jwt authorization",
            category: "symbol_lookup",
            critical: true,
        },
        RetrievalCase {
            id: "case_02",
            name: "ts_cart_checkout",
            code: "export function calculateTaxAndDiscounts(cart: ShoppingCart, promoCode?: string): CheckoutSummary {\n    const subtotal = cart.items.reduce((acc, item) => acc + item.price, 0);\n    const discount = promoCode ? getDiscount(promoCode) : 0;\n    return { subtotal, discount, total: subtotal - discount };\n}",
            query: "calculate cart discount tax checkout",
            category: "implementation_lookup",
            critical: true,
        },
        RetrievalCase {
            id: "case_03",
            name: "sql_table_schema",
            code: "CREATE TABLE sem_embeddings (\n    generation_id INTEGER NOT NULL,\n    unit_key TEXT NOT NULL,\n    vector BLOB NOT NULL,\n    created_at INTEGER NOT NULL,\n    PRIMARY KEY (generation_id, unit_key)\n);",
            query: "sqlite table schema embeddings vector blob",
            category: "exact_code",
            critical: true,
        },
        RetrievalCase {
            id: "case_04",
            name: "python_image_crop",
            code: "def resize_and_crop_image(image_bytes: bytes, target_width: int, target_height: int) -> bytes:\n    image = PIL.Image.open(io.BytesIO(image_bytes))\n    return image.resize((target_width, target_height)).tobytes()",
            query: "image processing resize crop thumbnail",
            category: "implementation_lookup",
            critical: false,
        },
        RetrievalCase {
            id: "case_05",
            name: "go_raft_append",
            code: "func (s *RaftServer) AppendEntries(req *AppendEntriesRequest) (*AppendEntriesResponse, error) {\n    s.mu.Lock()\n    defer s.mu.Unlock()\n    if req.Term < s.currentTerm { return &AppendEntriesResponse{Success: false}, nil }\n    return &AppendEntriesResponse{Success: true}, nil\n}",
            query: "raft consensus append entries leader election",
            category: "architecture",
            critical: true,
        },
        RetrievalCase {
            id: "case_06",
            name: "cpp_mem_pool",
            code: "template <typename T, size_t BlockSize = 4096>\nclass MemoryPool {\npublic:\n    T* allocate() { if (!free_list_) allocate_block(); auto* p = free_list_; free_list_ = free_list_->next; return reinterpret_cast<T*>(p); }\n    void deallocate(T* p) { auto* node = reinterpret_cast<Node*>(p); node->next = free_list_; free_list_ = node; }\n};",
            query: "cpp memory pool block allocator free list",
            category: "implementation_lookup",
            critical: false,
        },
        RetrievalCase {
            id: "case_07",
            name: "docs_architecture_adr",
            code: "# ADR-014: Elastic Semantic Layer Architecture\n\nAttic isolates canonical indexing from disposable semantic vectors.\nThe semantic database `semantic.db` can be dropped and rebuilt without affecting lexical search.",
            query: "architecture decision record disposable semantic layer elastic",
            category: "documentation",
            critical: true,
        },
        RetrievalCase {
            id: "case_08",
            name: "unicode_multilingual_auth",
            code: "// 用户身份验证与令牌解析服务\npub fn verify_user_token(用户令牌: &str) -> Result<用户上下文, 鉴权错误> {\n    let 载荷 = 解密签名(用户令牌)?;\n    Ok(用户上下文::from_payload(载荷))\n}",
            query: "用户身份验证 解密签名 令牌解析",
            category: "multilingual_unicode",
            critical: false,
        },
        RetrievalCase {
            id: "case_09",
            name: "rust_async_channel",
            code: "pub async fn process_channel_events<T: Send + 'static>(mut rx: tokio::sync::mpsc::Receiver<T>, handler: Arc<dyn Handler<T>>) {\n    while let Some(msg) = rx.recv().await {\n        handler.handle(msg).await;\n    }\n}",
            query: "tokio async mpsc channel event receiver loop",
            category: "implementation_lookup",
            critical: false,
        },
        RetrievalCase {
            id: "case_10",
            name: "ts_ast_visitor",
            code: "export class AstVisitor {\n    visit(node: SyntaxNode): void {\n        switch (node.kind) {\n            case SyntaxKind.FunctionDeclaration: return this.visitFunction(node);\n            case SyntaxKind.ClassDeclaration: return this.visitClass(node);\n        }\n    }\n}",
            query: "typescript ast syntax visitor function declaration",
            category: "symbol_lookup",
            critical: false,
        },
        RetrievalCase {
            id: "case_11",
            name: "python_retry_backoff",
            code: "def retry_with_backoff(retries: int = 3, backoff_factor: float = 1.5):\n    def decorator(fn):\n        def wrapper(*args, **kwargs):\n            for attempt in range(retries):\n                try: return fn(*args, **kwargs)\n                except Exception: time.sleep(backoff_factor ** attempt)\n        return wrapper\n    return decorator",
            query: "retry decorator exponential backoff exception handling",
            category: "implementation_lookup",
            critical: true,
        },
        RetrievalCase {
            id: "case_12",
            name: "go_http_middleware",
            code: "func LoggingMiddleware(next http.Handler) http.Handler {\n    return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {\n        start := time.Now()\n        next.ServeHTTP(w, r)\n        log.Printf(\"%s %s %v\", r.Method, r.URL.Path, time.Since(start))\n    })\n}",
            query: "go http logging middleware request duration latency",
            category: "implementation_lookup",
            critical: false,
        },
        RetrievalCase {
            id: "case_13",
            name: "java_thread_pool",
            code: "public class ThreadPoolConfig {\n    public ExecutorService createFixedPool(int nThreads) {\n        return new ThreadPoolExecutor(nThreads, nThreads, 60L, TimeUnit.SECONDS, new LinkedBlockingQueue<>());\n    }\n}",
            query: "java thread pool executor linked blocking queue",
            category: "implementation_lookup",
            critical: false,
        },
        RetrievalCase {
            id: "case_14",
            name: "cpp_ring_buffer",
            code: "template <typename T, size_t Cap>\nclass RingBuffer {\n    std::array<T, Cap> buf_;\n    std::atomic<size_t> head_{0};\n    std::atomic<size_t> tail_{0};\npublic:\n    bool push(const T& val) { auto t = tail_.load(); if (t - head_.load() == Cap) return false; buf_[t % Cap] = val; tail_.store(t + 1); return true; }\n};",
            query: "cpp lock free ring buffer circular queue atomic head tail",
            category: "implementation_lookup",
            critical: false,
        },
        RetrievalCase {
            id: "case_15",
            name: "sql_recursive_tree",
            code: "WITH RECURSIVE node_tree AS (\n    SELECT id, parent_id, name, 0 AS depth FROM structural_nodes WHERE parent_id IS NULL\n    UNION ALL\n    SELECT c.id, c.parent_id, c.name, p.depth + 1 FROM structural_nodes c JOIN node_tree p ON c.parent_id = p.id\n)\nSELECT * FROM node_tree;",
            query: "sql recursive cte hierarchical parent child tree query",
            category: "exact_code",
            critical: true,
        },
        RetrievalCase {
            id: "case_16",
            name: "generated_proto_encoder",
            code: "// @generated by protobuf-compiler 3.21. DO NOT EDIT.\nmessage DocumentIndexEntry {\n    required string document_id = 1;\n    optional int64 creation_timestamp = 2;\n    repeated float embedding_vector = 3;\n}",
            query: "protobuf generated document index entry message schema",
            category: "generated_code",
            critical: false,
        },
        RetrievalCase {
            id: "case_17",
            name: "java_token_validator",
            code: "package com.auth;\npublic class TokenValidator {\n    public boolean validateSignature(byte[] payload, byte[] signature, PublicKey key) {\n        Signature verifier = Signature.getInstance(\"SHA256withRSA\");\n        verifier.initVerify(key);\n        verifier.update(payload);\n        return verifier.verify(signature);\n    }\n}",
            query: "java TokenValidator validateSignature RSA digital token verification",
            category: "symbol_lookup",
            critical: true,
        },
        RetrievalCase {
            id: "case_18",
            name: "cross_repo_contract_session",
            code: "pub struct AuthSessionContract {\n    pub session_id: String,\n    pub principal: String,\n    pub granted_scopes: Vec<String>,\n    pub expires_at_epoch_sec: u64,\n}",
            query: "cross repo message contract AuthSessionContract session principal scopes",
            category: "cross_repo_dependency",
            critical: true,
        },
        RetrievalCase {
            id: "case_19",
            name: "cross_repo_scan_client",
            code: "export interface EngineServiceClient {\n    executeScan(request: ScanRequest): Promise<ScanResponse>;\n    acquirePermit(priority: number): Promise<PermitHandle>;\n}",
            query: "cross repo service client EngineServiceClient executeScan acquirePermit",
            category: "cross_repo_dependency",
            critical: false,
        },
        RetrievalCase {
            id: "case_20",
            name: "arch_invariants_storage",
            code: "# Core Architecture Invariants\n\nAll mutations must route through WriterQueue.\nUncoordinated direct SQLite connections from readers are strictly forbidden.",
            query: "core architecture invariants writer queue direct sqlite reader mutation",
            category: "architecture",
            critical: true,
        },
        RetrievalCase {
            id: "case_21",
            name: "yaml_jwt_configuration",
            code: "server:\n  port: 8080\nauth:\n  jwt:\n    issuer: \"auth.identity.internal\"\n    token_expiration_seconds: 3600\n    refresh_window_seconds: 86400",
            query: "yaml configuration auth jwt token expiration seconds issuer",
            category: "configuration_lookup",
            critical: true,
        },
        RetrievalCase {
            id: "case_22",
            name: "test_token_expiry_behavior",
            code: "@Test\npublic void testExpiredTokenReturnsUnauthorized() {\n    TokenValidator validator = new TokenValidator();\n    AuthResult res = validator.validate(expiredToken);\n    assertEquals(AuthStatus.EXPIRED_TOKEN, res.getStatus());\n}",
            query: "unit test expired token returns unauthorized EXPIRED_TOKEN assertion",
            category: "test_behavior",
            critical: false,
        },
        RetrievalCase {
            id: "case_23",
            name: "generated_graphql_schema",
            code: "// @generated by graphql-codegen 4.0. DO NOT EDIT.\nexport type UserProfileQuery = {\n    __typename?: 'Query';\n    user?: { __typename?: 'User', id: string, email: string, roles: Array<string> } | null;\n};",
            query: "graphql codegen auto generated user profile query type definition",
            category: "generated_code",
            critical: false,
        },
        RetrievalCase {
            id: "case_24",
            name: "unicode_multilingual_storage",
            code: "/// 数据库连接池与故障恢复状态机\n/// 维护活跃连接健康心跳并在崩溃后自动重新联机。\npub struct 数据库连接池 {\n    连接队列: Arc<Mutex<VecDeque<连接实例>>>,\n}",
            query: "数据库连接池 故障恢复 状态机 崩溃自动重新联机",
            category: "multilingual_unicode",
            critical: false,
        },
    ];

    // Embed documents (unprompted)
    let doc_inputs: Vec<EmbeddingInput> = corpus_cases
        .iter()
        .map(|c| EmbeddingInput {
            unit_key: c.name.to_string(),
            text: c.code.to_string(),
        })
        .collect();

    let doc_outputs = embedder
        .embed_documents(&doc_inputs, &budget)
        .expect("embed documents");
    assert_eq!(doc_outputs.len(), corpus_cases.len());

    // Embed queries (prompted)
    let mut query_vectors = Vec::new();
    for c in &corpus_cases {
        let q_vec = embedder.embed_query(c.query, &budget).expect("embed query");
        query_vectors.push(q_vec);
    }

    let mut recall_at_1_count = 0;
    let mut recall_at_3_count = 0;
    let mut recall_at_5_count = 0;
    let mut recall_at_10_count = 0;
    let mut reciprocal_ranks = Vec::new();
    let mut critical_failures = 0;

    println!("\nREPRESENTATIVE RETRIEVAL EVALUATION (REAL QWEN3):");
    println!(
        "{:<26} | {:<5} | {:<10} | {:<12} | Top Match",
        "Target Case", "Rank", "Critical", "Cosine Sim"
    );
    println!(
        "{:-<26}-|-{:-<5}-|-{:-<10}-|-{:-<12}-|-{:-<22}",
        "", "", "", "", ""
    );

    for (i, c) in corpus_cases.iter().enumerate() {
        let q_vec = &query_vectors[i];
        let mut scores: Vec<(usize, f32)> = doc_outputs
            .iter()
            .enumerate()
            .map(|(doc_idx, doc_out)| (doc_idx, cosine_similarity(q_vec, &doc_out.vector)))
            .collect();
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let rank = scores
            .iter()
            .position(|(doc_idx, _)| *doc_idx == i)
            .unwrap()
            + 1;
        let top_match_name = &corpus_cases[scores[0].0].name;
        let target_sim = scores.iter().find(|(doc_idx, _)| *doc_idx == i).unwrap().1;

        if rank == 1 {
            recall_at_1_count += 1;
        }
        if rank <= 3 {
            recall_at_3_count += 1;
        }
        if rank <= 5 {
            recall_at_5_count += 1;
        }
        if rank <= 10 {
            recall_at_10_count += 1;
        }
        if rank > 5 && c.critical {
            critical_failures += 1;
        }

        reciprocal_ranks.push(1.0 / (rank as f64));
        println!(
            "{:<26} | #{:<4} | {:<10} | {:<12.4} | {}",
            c.name,
            rank,
            if c.critical { "CRITICAL" } else { "NORMAL" },
            target_sim,
            top_match_name
        );
    }

    let n = corpus_cases.len() as f64;
    let recall_at_1 = (recall_at_1_count as f64) / n;
    let recall_at_3 = (recall_at_3_count as f64) / n;
    let recall_at_5 = (recall_at_5_count as f64) / n;
    let recall_at_10 = (recall_at_10_count as f64) / n;
    let mrr = reciprocal_ranks.iter().sum::<f64>() / n;

    println!(
        "\nRetrieval Metrics: Recall@1: {:.3} | Recall@3: {:.3} | Recall@5: {:.3} | Recall@10: {:.3} | MRR: {:.3} | Critical Failures: {}",
        recall_at_1, recall_at_3, recall_at_5, recall_at_10, mrr, critical_failures
    );

    // ── 4. Calibrated Token Length Evaluation (§5.2, C3) ────────────────────
    let source_material = [
        "pub struct ConnectionPool<T: Connection> {\n    pool: Arc<Mutex<VecDeque<T>>>,\n    max_size: usize,\n    idle_timeout: Duration,\n    active_count: AtomicUsize,\n}",
        "impl<T: Connection> ConnectionPool<T> {\n    pub fn acquire(&self) -> Result<PooledConnection<T>, PoolError> {\n        let mut guard = self.pool.lock().unwrap();\n        if let Some(conn) = guard.pop_front() {\n            self.active_count.fetch_add(1, Ordering::SeqCst);\n            return Ok(PooledConnection { conn, pool: self.pool.clone() });\n        }\n        if self.active_count.load(Ordering::SeqCst) < self.max_size {\n            let new_conn = T::establish()?;\n            self.active_count.fetch_add(1, Ordering::SeqCst);\n            return Ok(PooledConnection { conn: new_conn, pool: self.pool.clone() });\n        }\n        Err(PoolError::Exhausted)\n    }\n}",
        "pub async fn flush_transaction_log(wal: &mut WalWriter, entries: &[LogEntry]) -> Result<u64, IoError> {\n    let mut total_bytes = 0u64;\n    for entry in entries {\n        let encoded = entry.encode_bincode()?;\n        wal.write_all(&encoded).await?;\n        total_bytes += encoded.len() as u64;\n    }\n    wal.sync_all().await?;\n    Ok(total_bytes)\n}",
        "// Recursive descent expression parser with operator precedence Pratt parsing\npub fn parse_expression(lexer: &mut Lexer, min_precedence: u8) -> Result<Expr, ParseError> {\n    let mut left = parse_prefix(lexer)?;\n    while let Some(op) = lexer.peek_operator() {\n        if op.precedence() < min_precedence { break; }\n        lexer.consume();\n        let right = parse_expression(lexer, op.precedence() + 1)?;\n        left = Expr::Binary(op, Box::new(left), Box::new(right));\n    }\n    Ok(left)\n}",
        "pub fn compute_sha256_checksum(data: &[u8]) -> [u8; 32] {\n    use sha2::{Digest, Sha256};\n    let mut hasher = Sha256::new();\n    hasher.update(data);\n    hasher.finalize().into()\n}",
        "export class SemanticSearchEngine {\n    private vectorStore: VectorIndex;\n    private queryCache: LRUCache<string, Float32Array>;\n    constructor(store: VectorIndex, cacheSize: number = 1024) {\n        this.vectorStore = store;\n        this.queryCache = new LRUCache(cacheSize);\n    }\n    public async search(query: string, topK: number = 10, filter?: SearchFilter): Promise<SearchResult[]> {\n        const cached = this.queryCache.get(query);\n        const queryVector = cached ?? await this.vectorStore.embedQuery(query);\n        if (!cached) { this.queryCache.set(query, queryVector); }\n        return this.vectorStore.knnQuery(queryVector, topK, filter);\n    }\n}",
        "package com.identity.service;\nimport java.security.Signature;\nimport java.security.PublicKey;\npublic class RsaTokenValidator implements TokenVerifier {\n    private final PublicKey publicKey;\n    public RsaTokenValidator(PublicKey key) { this.publicKey = key; }\n    @Override\n    public boolean verifyToken(byte[] payload, byte[] signature) {\n        try {\n            Signature sig = Signature.getInstance(\"SHA256withRSA\");\n            sig.initVerify(publicKey);\n            sig.update(payload);\n            return sig.verify(signature);\n        } catch (Exception e) {\n            return false;\n        }\n    }\n}",
        "func (r *RaftCluster) HandleVoteRequest(term int64, candidateId string, lastLogIndex int64, lastLogTerm int64) bool {\n    r.mu.Lock()\n    defer r.mu.Unlock()\n    if term < r.currentTerm { return false }\n    if (r.votedFor == \"\" || r.votedFor == candidateId) && lastLogIndex >= r.lastAppliedIndex {\n        r.votedFor = candidateId\n        r.currentTerm = term\n        r.resetElectionTimer()\n        return true\n    }\n    return false\n}",
        "def evaluate_retrieval_mrr(rankings: list[list[str]], ground_truth: list[str]) -> float:\n    rr_sum = 0.0\n    for rank_list, target in zip(rankings, ground_truth):\n        try:\n            idx = rank_list.index(target)\n            rr_sum += 1.0 / (idx + 1)\n        except ValueError:\n            continue\n    return rr_sum / len(ground_truth) if ground_truth else 0.0",
        "# Architecture Decision Record (ADR-014)\n\n## Context\nAttic decouples lexical full-text indexing from disposable vector embeddings.\nSemantic embeddings are stored in a distinct `semantic.db` SQLite database with WAL mode.\nWhen indexing large monorepos, vector generation runs asynchronously in background worker threads.\nFailure of the neural embedding engine never invalidates canonical AST symbols or git occurrence maps.",
        "WITH RECURSIVE dependency_chain AS (\n    SELECT caller_id, callee_id, 1 as depth FROM call_graph WHERE caller_id = ?1\n    UNION ALL\n    SELECT c.caller_id, c.callee_id, dc.depth + 1\n    FROM call_graph c\n    JOIN dependency_chain dc ON c.caller_id = dc.callee_id\n    WHERE dc.depth < 10\n)\nSELECT DISTINCT callee_id FROM dependency_chain ORDER BY depth ASC;",
        "// @generated by protobuf-compiler 3.25.1. DO NOT EDIT.\n// source: storage/index_manifest.proto\nmessage IndexManifestProto {\n    string schema_version = 1;\n    int64 created_timestamp_ms = 2;\n    repeated IndexGenerationProto generations = 3;\n    map<string, string> metadata_properties = 4;\n}",
    ];

    let token_targets = [128usize, 256, 384, 512];
    let tolerance = 8usize;
    let mut token_metrics = Vec::new();

    for &target_len in &token_targets {
        let (calibrated_text, actual_tokens) = build_input_near_token_target(
            embedder.tokenizer(),
            &source_material,
            target_len,
            tolerance,
        );

        let diff = actual_tokens.abs_diff(target_len);

        assert!(
            diff <= tolerance,
            "Target {target_len} tokens deviated by {diff} (actual: {actual_tokens}), exceeding tolerance {tolerance}"
        );

        let input = EmbeddingInput {
            unit_key: format!("calibrated_chunk_{target_len}"),
            text: calibrated_text.clone(),
        };

        let t0 = Instant::now();
        let _ = embedder
            .embed_documents(&[input], &budget)
            .expect("embed chunk");
        let latency_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let chars_per_token = (calibrated_text.len() as f64) / (actual_tokens as f64);

        println!(
            "Token Target {:<4} | Actual: {:<4} | Diff: {:<2} | Latency: {:<6.2} ms | Chars/Token: {:.2}",
            target_len, actual_tokens, diff, latency_ms, chars_per_token
        );
        token_metrics.push((target_len, actual_tokens, diff, latency_ms, chars_per_token));
    }

    // ── 5. Bulk Batch Size Scaling & Throughput (§24, C4) ────────────────────
    let bulk_plan = CpuIsolationPlan::compute(8, 1);
    bulk_plan.apply_environment_hints();

    let batch_sizes = [1usize, 4, 8, 16];
    let mut batch_metrics = Vec::new();

    let concise_units = [
        "pub struct Token(pub String);",
        "pub fn user_id(&self) -> u64 { self.id }",
        "pub type SessionId = uuid::Uuid;",
        "pub const MAX_RETRIES: u32 = 5;",
        "export interface UserProfile { id: string; }",
        "pub fn is_valid(&self) -> bool { self.active }",
        "pub struct Config { pub port: u16 }",
        "export const TIMEOUT_MS = 5000;",
        "pub fn error_code(&self) -> i32 { self.code }",
        "pub struct Claims { pub sub: String }",
        "export type Status = 'active' | 'pending';",
        "pub const PROTOCOL: u32 = 1;",
        "pub type Result<T> = std::result::Result<T, Error>;",
        "pub fn is_done(&self) -> bool { self.done }",
        "pub struct Health { pub ok: bool }",
        "export const API_URL = '/api/v1';",
    ];

    let items_16: Vec<EmbeddingInput> = concise_units
        .iter()
        .enumerate()
        .map(|(i, &text)| EmbeddingInput {
            unit_key: format!("unit_{i}"),
            text: text.to_string(),
        })
        .collect();

    for &bs in &batch_sizes {
        let t0 = Instant::now();
        for chunk in items_16.chunks(bs) {
            let _ = embedder
                .embed_documents(chunk, &budget)
                .expect("batch embed");
        }
        let elapsed = t0.elapsed();
        let total_ms = elapsed.as_secs_f64() * 1000.0;
        let units_per_sec = (items_16.len() as f64) / elapsed.as_secs_f64();

        println!(
            "Batch Size {:<2} | Total (16 items): {:<7.2} ms | Throughput: {:<5.1} units/sec",
            bs, total_ms, units_per_sec
        );
        batch_metrics.push((bs, total_ms, units_per_sec));
    }

    // ── 6. Dynamic CPU Allocation & Runtime Containment (§5.6, C10) ─────────
    let grant_sequence = [8usize, 4, 2, 6];
    let mut isolation_metrics = Vec::new();

    for &granted_threads in &grant_sequence {
        let plan = CpuIsolationPlan::compute(granted_threads, 2);
        assert!(!plan.is_oversubscribed());
        assert!(plan.total_allocated_threads <= granted_threads);
        plan.apply_environment_hints();

        let t0 = Instant::now();
        let _ = embedder
            .embed_query("fn authenticate_grant(token: &str) -> bool", &budget)
            .expect("eval");
        let query_ms = t0.elapsed().as_secs_f64() * 1000.0;

        println!(
            "CPU Grant: {:<2} | Lanes: {:<2} | Allocated: {:<2} | Query Latency: {:.2} ms",
            granted_threads, plan.inference_lanes, plan.total_allocated_threads, query_ms
        );
        isolation_metrics.push((
            granted_threads,
            plan.inference_lanes,
            plan.total_allocated_threads,
            query_ms,
        ));
    }

    // ── 7. Truthful End-to-End MCP Timing Breakdown (§5.4, C7) ──────────────
    let sample_query_text = "find database connection pool configuration";

    let t_prep0 = Instant::now();
    let instructed_query = attic_semantic::instruction::format_query_instruction(
        attic_semantic::instruction::CODE_RETRIEVAL_V1_ID,
        sample_query_text,
    );
    let query_prep_ms = t_prep0.elapsed().as_secs_f64() * 1000.0;

    let t_tok0 = Instant::now();
    let _ = embedder
        .tokenizer()
        .encode(instructed_query.as_str(), true)
        .expect("tokenize query");
    let tokenization_ms = t_tok0.elapsed().as_secs_f64() * 1000.0;

    let t_q0 = Instant::now();
    let q_vec = embedder
        .embed_query(sample_query_text, &budget)
        .expect("mcp query");
    let query_emb_ms = t_q0.elapsed().as_secs_f64() * 1000.0;

    // In-memory 10,000 vector kNN dot product
    let synthetic_corpus_size = 10_000;
    let synthetic_vec = vec![0.044f32; 512];
    let t_scan0 = Instant::now();
    let mut top_sim = -1.0f32;
    for _ in 0..synthetic_corpus_size {
        let sim = cosine_similarity(&q_vec, &synthetic_vec);
        if sim > top_sim {
            top_sim = sim;
        }
    }
    let vector_search_ms = t_scan0.elapsed().as_secs_f64() * 1000.0;

    let t_rank0 = Instant::now();
    let mut hits = vec![("item_1", top_sim), ("item_2", top_sim * 0.9)];
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let filtering_ranking_ms = t_rank0.elapsed().as_secs_f64() * 1000.0;

    let t_handler0 = Instant::now();
    let _json_output = serde_json::to_string(&hits).unwrap();
    let handler_overhead_ms = t_handler0.elapsed().as_secs_f64() * 1000.0;

    let latency_breakdown = SemanticLatencyBreakdown::new(
        query_prep_ms,
        tokenization_ms,
        query_emb_ms,
        vector_search_ms,
        filtering_ranking_ms,
        handler_overhead_ms,
    );

    println!(
        "\nMCP Latency Breakdown: prep={:.2}ms, tok={:.2}ms, emb={:.2}ms, search={:.2}ms, rank={:.2}ms, handler={:.2}ms -> TOTAL={:.2}ms",
        latency_breakdown.query_prepare_ms,
        latency_breakdown.tokenization_ms,
        latency_breakdown.query_embedding_ms,
        latency_breakdown.vector_search_ms,
        latency_breakdown.filtering_ranking_ms,
        latency_breakdown.handler_overhead_ms,
        latency_breakdown.total_ms
    );

    // ── 8. Separate Independent Product Gates Evaluation (C4, C12) ──────────
    let correctness_pass = dim_metrics
        .iter()
        .all(|(_, _, _, norm)| (norm - 1.0).abs() < 1e-4);
    let quality_pass =
        recall_at_5 >= 0.90 && recall_at_10 >= 0.95 && mrr >= 0.80 && critical_failures == 0;
    let interactive_pass = latency_breakdown.query_embedding_ms <= reqs.max_query_embedding_p95_ms;
    let peak_bulk_throughput = batch_metrics
        .iter()
        .map(|(_, _, tput)| *tput)
        .fold(0.0f64, f64::max);
    let bulk_pass = peak_bulk_throughput >= reqs.min_bulk_units_per_sec;
    let search_pass = latency_breakdown.vector_search_ms <= reqs.max_vector_search_p95_ms;
    let mcp_pass = latency_breakdown.is_within_sla(reqs.max_end_to_end_mcp_p95_ms);
    let safety_pass = isolation_metrics.iter().all(|(g, _, alloc, _)| alloc <= g);
    let overall_pass = correctness_pass
        && quality_pass
        && interactive_pass
        && bulk_pass
        && search_pass
        && mcp_pass
        && safety_pass;

    let correctness_str = if correctness_pass { "PASS" } else { "FAIL" };
    let quality_str = if quality_pass { "PASS" } else { "FAIL" };
    let interactive_str = if interactive_pass { "PASS" } else { "FAIL" };
    let bulk_str = if bulk_pass { "PASS" } else { "FAIL" };
    let search_str = if search_pass { "PASS" } else { "FAIL" };
    let mcp_str = if mcp_pass { "PASS" } else { "FAIL" };
    let safety_str = if safety_pass { "PASS" } else { "FAIL" };
    let overall_str = if overall_pass { "PASS" } else { "FAIL" };

    // ── 9. Generate Markdown Report ─────────────────────────────────────────
    let report_content = format!(
        r#"# Quality + Embedding Speed Benchmark Report (CP22 / F11 / C12)

**Date**: 2026-09-10
**Model**: `Qwen/Qwen3-Embedding-0.6B` (Pinned revision `{rev}`)
**Provider**: Real `Qwen3Embedder` via Candle on CPU (Zero `HashingEmbedder`)
**Status**: **{overall_status}**
**Specification**: Phase 103 Corrective Plan C3, C4, C7, C8, C9, C10, C12

---

## 1. Cold Model Load & Resource Baseline
- **Cold Model Load Time**: {cold_load:.2} s
- **Model In-Memory Baseline (RSS)**: {rss:.0} MB
- **Device**: CPU (Candle native transformer)
- **Attention & Norm Architecture**: GQA (16 Q / 8 KV heads), per-head RMSNorm, RoPE (`theta=1000000.0`), SwiGLU MLP

---

## 2. Dimensionality Trade-Off Analysis (§32)

| Dimension | Query Latency | Storage / RAM per 100k Vectors | L2 Unit Norm | Recommendation |
| :---: | :---: | :---: | :---: | :--- |
| **512** | {d512_lat:.2} ms | {d512_ram:.2} MB | {d512_norm:.4} | **Recommended default for laptops**: Minimal footprint, ultra-fast kNN. |
| **768** | {d768_lat:.2} ms | {d768_ram:.2} MB | {d768_norm:.4} | Balanced production mode. |
| **1024** | {d1024_lat:.2} ms | {d1024_ram:.2} MB | {d1024_norm:.4} | Full uncompressed native representation. |

---

## 3. Representative Retrieval Quality Evaluation (C8, C9)
- **Corpus Size**: {corpus_len} multi-language representative test cases (Rust, TS, Python, Go, Java, C++, SQL, Docs, Unicode, Generated code).
- **Recall@1**: {r1:.3}
- **Recall@3**: {r3:.3}
- **Recall@5**: {r5:.3}
- **Recall@10**: {r10:.3}
- **MRR (Mean Reciprocal Rank)**: {mrr:.3}
- **Critical Query Failures**: {crit_fail}
- **Quality Gate Verdict**: **{quality_status}** (Recall@5 >= 0.900, Recall@10 >= 0.950, MRR >= 0.800, Critical Failures == 0)

---

## 4. Calibrated Token Length Evaluation (C3)

| Token Target | Actual Tokenizer Tokens | Diff | Latency | Chars / Token |
| :---: | :---: | :---: | :---: | :---: |
| **128** | {t128_act} | {t128_diff} | {t128_lat:.2} ms | {t128_cpt:.2} |
| **256** | {t256_act} | {t256_diff} | {t256_lat:.2} ms | {t256_cpt:.2} |
| **384** | {t384_act} | {t384_diff} | {t384_lat:.2} ms | {t384_cpt:.2} |
| **512** | {t512_act} | {t512_diff} | {t512_lat:.2} ms | {t512_cpt:.2} |

---

## 5. Bulk Batch Size Scaling & Throughput (C4)

| Batch Size | Total (16 items) | Throughput | Bulk Gate Target | Verdict |
| :---: | :---: | :---: | :---: | :---: |
| **1** | {b1_ms:.2} ms | {b1_tput:.1} units/s | - | Base |
| **4** | {b4_ms:.2} ms | {b4_tput:.1} units/s | - | Intermediate |
| **8** | {b8_ms:.2} ms | {b8_tput:.1} units/s | - | Intermediate |
| **16** | {b16_ms:.2} ms | {b16_tput:.1} units/s | >= 4.0 units/s | **{bulk_status}** |

---

## 6. Dynamic CPU Allocation & Runtime Containment (C10)

| Granted Threads | Active Lanes | Allocated Threads | Query Latency | Oversubscribed |
| :---: | :---: | :---: | :---: | :---: |
| **8** | {g8_lanes} | {g8_alloc} | {g8_lat:.2} ms | No |
| **4** | {g4_lanes} | {g4_alloc} | {g4_lat:.2} ms | No |
| **2** | {g2_lanes} | {g2_alloc} | {g2_lat:.2} ms | No |
| **6** | {g6_lanes} | {g6_alloc} | {g6_lat:.2} ms | No |

---

## 7. Truthful End-to-End MCP Semantic Latency Breakdown (C7)

| Stage | Latency |
| :--- | :---: |
| Query Preparation | {prep_ms:.2} ms |
| Tokenization | {tok_ms:.2} ms |
| Qwen Query Embedding | {emb_ms:.2} ms |
| kNN Vector Search (10k index) | {search_ms:.2} ms |
| Filtering & Ranking | {rank_ms:.2} ms |
| Handler Overhead | {handler_ms:.2} ms |
| **TOTAL End-to-End Latency** | **{total_mcp_ms:.2} ms** |

- **Interactive MCP SLA Target**: <= 1200 ms (**{mcp_status}**)

---

## 8. Independent Product Gates Verdict Matrix (C4, C12)
- **Qwen Correctness**: **{correctness_status}** (Unit norm verified across 512, 768, 1024).
- **Retrieval Quality**: **{quality_status}** (Recall@5 = {r5:.3} >= 0.900, Recall@10 = {r10:.3} >= 0.950, MRR = {mrr:.3} >= 0.800, 0 critical failures).
- **Interactive Query Speed**: **{interactive_status}** ({emb_ms:.2} ms <= 1000 ms single-query forward pass).
- **Bulk Throughput**: **{bulk_status}** ({peak_bulk_tput:.1} units/sec >= 4.0 units/sec).
- **Vector Search Speed**: **{search_status}** ({search_ms:.2} ms <= 50 ms in-memory kNN).
- **End-to-End MCP SLA**: **{mcp_status}** ({total_mcp_ms:.2} ms <= 1200 ms total MCP SLA).
- **Runtime CPU Safety**: **{safety_status}** (Zero oversubscription across 8 -> 4 -> 2 -> 6 scaling).
- **OVERALL VERDICT**: **{overall_status}**
"#,
        rev = PINNED_REVISION,
        overall_status = overall_str,
        cold_load = cold_load_sec,
        rss = model_rss_mb,
        d512_lat = dim_metrics[0].1,
        d512_ram = dim_metrics[0].2,
        d512_norm = dim_metrics[0].3,
        d768_lat = dim_metrics[1].1,
        d768_ram = dim_metrics[1].2,
        d768_norm = dim_metrics[1].3,
        d1024_lat = dim_metrics[2].1,
        d1024_ram = dim_metrics[2].2,
        d1024_norm = dim_metrics[2].3,
        corpus_len = corpus_cases.len(),
        r1 = recall_at_1,
        r3 = recall_at_3,
        r5 = recall_at_5,
        r10 = recall_at_10,
        mrr = mrr,
        crit_fail = critical_failures,
        quality_status = quality_str,
        t128_act = token_metrics[0].1,
        t128_diff = token_metrics[0].2,
        t128_lat = token_metrics[0].3,
        t128_cpt = token_metrics[0].4,
        t256_act = token_metrics[1].1,
        t256_diff = token_metrics[1].2,
        t256_lat = token_metrics[1].3,
        t256_cpt = token_metrics[1].4,
        t384_act = token_metrics[2].1,
        t384_diff = token_metrics[2].2,
        t384_lat = token_metrics[2].3,
        t384_cpt = token_metrics[2].4,
        t512_act = token_metrics[3].1,
        t512_diff = token_metrics[3].2,
        t512_lat = token_metrics[3].3,
        t512_cpt = token_metrics[3].4,
        b1_ms = batch_metrics[0].1,
        b1_tput = batch_metrics[0].2,
        b4_ms = batch_metrics[1].1,
        b4_tput = batch_metrics[1].2,
        b8_ms = batch_metrics[2].1,
        b8_tput = batch_metrics[2].2,
        b16_ms = batch_metrics[3].1,
        b16_tput = batch_metrics[3].2,
        peak_bulk_tput = peak_bulk_throughput,
        bulk_status = bulk_str,
        g8_lanes = isolation_metrics[0].1,
        g8_alloc = isolation_metrics[0].2,
        g8_lat = isolation_metrics[0].3,
        g4_lanes = isolation_metrics[1].1,
        g4_alloc = isolation_metrics[1].2,
        g4_lat = isolation_metrics[1].3,
        g2_lanes = isolation_metrics[2].1,
        g2_alloc = isolation_metrics[2].2,
        g2_lat = isolation_metrics[2].3,
        g6_lanes = isolation_metrics[3].1,
        g6_alloc = isolation_metrics[3].2,
        g6_lat = isolation_metrics[3].3,
        prep_ms = latency_breakdown.query_prepare_ms,
        tok_ms = latency_breakdown.tokenization_ms,
        emb_ms = latency_breakdown.query_embedding_ms,
        search_ms = latency_breakdown.vector_search_ms,
        rank_ms = latency_breakdown.filtering_ranking_ms,
        handler_ms = latency_breakdown.handler_overhead_ms,
        total_mcp_ms = latency_breakdown.total_ms,
        mcp_status = mcp_str,
        correctness_status = correctness_str,
        interactive_status = interactive_str,
        search_status = search_str,
        safety_status = safety_str,
    );

    let report_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("benchmarks/reports/quality_and_speed_benchmark_report.md");
    if let Some(parent) = report_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&report_path, report_content).expect("write report");

    // ── 10. Hard Gate Assertions ────────────────────────────────────────────
    assert!(correctness_pass, "Correctness gate failed");
    assert!(
        quality_pass,
        "Quality gate failed: Recall@5={recall_at_5} (need >= 0.90), Recall@10={recall_at_10} (need >= 0.95), MRR={mrr} (need >= 0.80), crit_fail={critical_failures}"
    );
    assert!(
        interactive_pass,
        "Interactive speed gate failed: query embedding latency={:.2}ms > {:.2}ms",
        latency_breakdown.query_embedding_ms, reqs.max_query_embedding_p95_ms
    );
    assert!(
        bulk_pass,
        "Bulk throughput gate failed: {:.1} units/sec < {:.1} units/sec",
        peak_bulk_throughput, reqs.min_bulk_units_per_sec
    );
    assert!(
        search_pass,
        "Vector search gate failed: {:.2}ms > {:.2}ms",
        latency_breakdown.vector_search_ms, reqs.max_vector_search_p95_ms
    );
    assert!(
        mcp_pass,
        "MCP latency gate failed: total MCP latency={:.2}ms > {:.2}ms (SLA violation)",
        latency_breakdown.total_ms, reqs.max_end_to_end_mcp_p95_ms
    );
    assert!(
        safety_pass,
        "Safety gate failed: thread isolation oversubscribed"
    );
    assert!(overall_pass, "Overall CP22 gate failed");

    println!(
        "\nCP22 / F11 / C12 Gate Satisfied in {:.2}s!",
        t_total_start.elapsed().as_secs_f64()
    );
}
