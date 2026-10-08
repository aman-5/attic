//! Central knowledge folder (`[knowledge] dir`): notes indexed as their own
//! repository must reach `context` answers for ANY repository, even when
//! stronger code matches would otherwise crowd them out, without changing
//! which code evidence is served.

use std::path::Path;

use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_retrieval::{AnswerMode, AnswerOutcome, AnswerRequest, RetrievalService};
use attic_storage::{DbPool, WriterQueue, WriterQueueHandle, open_db, run_migrations};
use tempfile::TempDir;

const CODE_FILES: usize = 60;
const NOTE: &str = "# Payment retry policy\n\nPayment retry policy: retries happen three \
                    times with exponential backoff; the payment retry policy is owned by \
                    the billing team.\n";
const README: &str = "# Knowledge folder\n\nPut payment retry policy notes here.\n";

struct TwoRepos {
    pool: DbPool,
    _queue: WriterQueue,
    writer: WriterQueueHandle,
    code_id: String,
    knowledge_id: String,
    _dir: TempDir,
}

fn write_all(root: &Path, files: &[(String, String)]) {
    std::fs::create_dir_all(root).unwrap();
    for (rel, body) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
}

fn index(pool: &DbPool, writer: &WriterQueueHandle, root: &Path, name: &str) -> String {
    let store = IndexingStore {
        readers: pool,
        writer,
    };
    let opts = IndexOptions {
        repository_name: name.into(),
        ..Default::default()
    };
    index_repository(&store, root, &DiscoveryPolicy::default_git(), &opts)
        .expect("indexing")
        .repository_id
}

fn setup(notes: &[(String, String)]) -> TwoRepos {
    let dir = TempDir::new().unwrap();
    let code_root = dir.path().join("code");
    let code: Vec<(String, String)> = (0..CODE_FILES)
        .map(|i| {
            (
                format!("src/payment_{i}.py"),
                format!(
                    "def payment_retry_policy_{i}():\n    \"\"\"payment retry policy payment \
                     retry policy\"\"\"\n    return 'payment retry policy'\n"
                ),
            )
        })
        .collect();
    write_all(&code_root, &code);
    let knowledge_root = dir.path().join("knowledge");
    write_all(&knowledge_root, notes);

    let db_path = dir.path().join("attic.db");
    let (conn, pool) = open_db(&db_path).unwrap();
    run_migrations(&conn).unwrap();
    let queue = WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let code_id = index(&pool, &writer, &code_root, "code");
    let knowledge_id = index(&pool, &writer, &knowledge_root, "attic-knowledge");
    TwoRepos {
        pool,
        _queue: queue,
        writer,
        code_id,
        knowledge_id,
        _dir: dir,
    }
}

impl TwoRepos {
    fn ask(&self, question: &str, central: bool) -> AnswerOutcome {
        let service = RetrievalService {
            readers: self.pool.clone(),
            writer: self.writer.clone(),
            semantic: None,
            crossrepo_degraded: false,
        };
        let mut req = AnswerRequest::new(question, AnswerMode::Normal);
        req.repository_ids = vec![self.code_id.clone()];
        if central {
            req.knowledge_repository_id = Some(self.knowledge_id.clone());
        }
        service.answer(&req).expect("answer")
    }

    fn knowledge_paths(&self, out: &AnswerOutcome) -> Vec<String> {
        out.served_evidence
            .iter()
            .filter(|e| e.repository_id == self.knowledge_id)
            .map(|e| e.path.clone())
            .collect()
    }
}

fn notes(n: usize) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = (0..n)
        .map(|i| (format!("note_{i}.md"), NOTE.to_string()))
        .collect();
    v.push(("README.md".into(), README.into()));
    v
}

// No "knowledge"/"ADR"/"runbook" wording, asked about the CODE repository,
// against 60 stronger code matches: the note must still be served.
#[test]
fn central_note_reaches_context_despite_crowding_code_and_no_trigger_words() {
    let fx = setup(&notes(1));
    let question = "How does the payment retry policy work?";

    let without = fx.ask(question, false);
    assert!(
        fx.knowledge_paths(&without).is_empty(),
        "baseline must not see the central folder"
    );

    let with = fx.ask(question, true);
    let served = fx.knowledge_paths(&with);
    assert_eq!(served, vec!["note_0.md".to_string()], "served={served:?}");
    let kn = with
        .served_evidence
        .iter()
        .find(|e| e.repository_id == fx.knowledge_id)
        .unwrap();
    assert_eq!(kn.source_type.as_str(), "KNOWLEDGE");
    let ctx = with.context_text.expect("context");
    assert!(ctx.contains("note_0.md"), "{ctx}");
    assert!(ctx.contains("PROJECT_KNOWLEDGE"), "{ctx}");
}

#[test]
fn central_knowledge_does_not_change_served_code() {
    let fx = setup(&notes(3));
    let question = "How does the payment retry policy work?";
    let code_paths = |out: &AnswerOutcome| {
        let mut p: Vec<String> = out
            .served_evidence
            .iter()
            .filter(|e| e.repository_id == fx.code_id)
            .map(|e| e.path.clone())
            .collect();
        p.sort();
        p
    };
    let without = fx.ask(question, false);
    let with = fx.ask(question, true);
    assert!(!code_paths(&without).is_empty());
    assert_eq!(code_paths(&without), code_paths(&with));
}

// A note that matches the question only weakly (one term, buried in other
// text) still reaches the answer: it is not judged against code scores.
#[test]
fn weakly_matching_central_note_is_still_served() {
    let filler = "Team calendar, office hours and onboarding checklist. ".repeat(40);
    let weak = format!("# Billing\n\n{filler}\nA retry is owned by billing.\n{filler}");
    let fx = setup(&[("billing.md".into(), weak)]);
    let with = fx.ask("Where is the payment retry policy defined?", true);
    assert_eq!(
        with.plan.query_type,
        attic_retrieval::QueryType::DefinitionLookup
    );
    assert_eq!(
        fx.knowledge_paths(&with),
        vec!["billing.md".to_string()],
        "dropped={:?}",
        with.plan.evidence_dropped
    );
}

#[test]
fn central_readme_is_never_served() {
    let fx = setup(&notes(1));
    let with = fx.ask("How does the payment retry policy work?", true);
    let served = fx.knowledge_paths(&with);
    assert!(
        !served.iter().any(|p| p.eq_ignore_ascii_case("readme.md")),
        "served={served:?}"
    );
}

#[test]
fn central_knowledge_is_capped() {
    let fx = setup(&notes(12));
    let with = fx.ask("How does the payment retry policy work?", true);
    let served = fx.knowledge_paths(&with);
    assert!(!served.is_empty());
    assert!(
        served.len() <= attic_retrieval::candidates::CENTRAL_KNOWLEDGE_LIMIT,
        "served {} notes",
        served.len()
    );
}
