//! TEMPORARY measurement probe — not part of the product; deleted after use.
//! Counts real Qwen3 tokens (capped at the worker's 512-token sequence
//! length) for every text in PROBE_JSONL.

#[test]
#[ignore = "manual probe"]
fn token_count_probe() {
    let path = std::env::var("PROBE_JSONL").expect("PROBE_JSONL");
    let tok_file = std::env::var("PROBE_TOKENIZER").expect("PROBE_TOKENIZER");
    let tokenizer = tokenizers::Tokenizer::from_file(&tok_file).expect("tokenizer.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let (mut items, mut tokens, mut capped, mut raw_bytes) = (0u64, 0u64, 0u64, 0u64);
    for line in text.lines() {
        let body: String = serde_json::from_str(line).unwrap();
        raw_bytes += body.len() as u64;
        let n = tokenizer.encode(body.as_str(), false).unwrap().len() as u64;
        tokens += n.min(512);
        capped += u64::from(n > 512);
        items += 1;
    }
    println!(
        "TOKENS {path}: items={items} tokens={tokens} items_over_512={capped} bytes={raw_bytes} bytes_per_token={:.2}",
        raw_bytes as f64 / tokens.max(1) as f64
    );
}
