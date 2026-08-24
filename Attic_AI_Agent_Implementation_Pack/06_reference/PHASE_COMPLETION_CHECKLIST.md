# Phase Completion Checklist

- [ ] Current phase scope implemented
- [ ] No future-phase leakage
- [ ] Contracts updated where implementation clarified them
- [ ] No architecture drift
- [ ] New dependencies documented and verified
- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Phase-specific integration tests pass
- [ ] Security invariants applicable to phase pass
- [ ] Failure paths tested
- [ ] No secret data in fixtures/log output
- [ ] Diff inspected
- [ ] Decisions recorded
- [ ] Open questions updated
- [ ] Benchmark slice run where required
- [ ] Phase gate explicitly recorded PASS
