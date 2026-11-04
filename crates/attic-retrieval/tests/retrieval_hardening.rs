mod common;

use attic_retrieval::{AnswerMode, AnswerModePolicy};
use common::Fixture;

#[test]
fn reranking_invoked_is_false_even_when_policy_permits_it() {
    let fx = Fixture::bootstrap();
    for mode in [AnswerMode::Normal, AnswerMode::Deep] {
        let out = fx.ask("retry_limit setting value", mode);
        assert!(
            !out.plan.policy_trace.reranking_invoked,
            "{mode}: permission is not execution — no reranker exists"
        );
        assert!(AnswerModePolicy::for_mode(mode).reranking_allowed);
    }
}
