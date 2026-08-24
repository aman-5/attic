# Production-Ready Workspace Code Intelligence MCP --- Final Canonical Plan

## 1. Objective

Build one production-grade MCP server that provides reliable engineering
intelligence across approximately 25--30 repositories and roughly 500K
cheap searchable retrieval units.

The system must support:

-   many programming languages, without an architectural language limit
-   structured data and configuration formats
-   documentation formats
-   infrastructure/build formats
-   custom and unknown formats
-   very large files
-   project-specific `knowledge/*.md`
-   cross-repository dependencies
-   uncommitted working-tree changes
-   incremental updates
-   Linux and macOS
-   bounded CPU/RAM/disk usage
-   crash recovery
-   reproducible retrieval
-   evidence-grounded answers
-   explicit insufficient-evidence behavior

The architectural principle is:

> **Build an evidence system that uses indexes, rather than an index
> that happens to answer questions.**

And preserve this separation:

``` text
SOURCE
  !=
INDEX
  !=
RETRIEVAL CANDIDATE
  !=
EVIDENCE
  !=
CONTEXT
  !=
ANSWER
```

Repositories remain the source of truth. All indexes, graphs,
embeddings, summaries, and caches are derived and replaceable.

------------------------------------------------------------------------

# 2. Success Criteria

The new MCP must improve over the existing KG-MCP on the target workload
in:

-   retrieval recall
-   answer correctness
-   groundedness
-   cross-repository questions
-   incremental indexing
-   resource control

It must show no material regression on:

-   exact lookup
-   symbol lookup
-   configuration lookup
-   simple file/content search

The actual 25--30 repository workspace is the primary benchmark.

External systems such as Graft may be evaluated as secondary baselines,
but they are not mandatory shipping gates.

------------------------------------------------------------------------

# 3. Canonical Architecture

``` text
SOURCE
  |
  +-- repository filesystem
  +-- Git state
  +-- working-tree changes
  +-- knowledge
  |
  v
SOURCE SNAPSHOT
  |
  v
DISCOVERY + SECURITY
  |
  v
CHANGE DETECTION
  |
  v
ANALYZER REGISTRY
  |
  v
CANONICAL MODEL
  |
  +-- files
  +-- structural nodes
  +-- symbols
  +-- relationships
  +-- knowledge
  |
  v
DERIVED INTELLIGENCE
  |
  +-- SQLite / FTS5
  +-- structural indexes
  +-- relationship graph
  +-- optional semantic index
  |
  v
QUERY
  |
  v
QUERY ROUTER
  |
  v
ANSWER MODE POLICY
  |
  v
QUERY EVIDENCE CONTRACT
  |
  v
RETRIEVAL PLANNER
  |
  v
RETRIEVAL PLAN
  |
  v
CANDIDATE GENERATION
  |
  +-- lexical
  +-- symbol
  +-- structural
  +-- knowledge
  +-- graph
  +-- semantic
  |
  v
CANDIDATE FUSION
  |
  v
EVIDENCE RANKING
  |
  v
EVIDENCE VALIDATION / MANAGER
  |
  +-- provenance
  +-- freshness
  +-- authority
  +-- confidence
  +-- contradictions
  +-- completeness
  |
  +------------ sufficient ------------+
  |                                    |
  | insufficient                       |
  v                                    |
TARGETED EXPANSION                     |
  |                                    |
  +-- broader indexed retrieval        |
  +-- bounded graph expansion          |
  +-- direct source verification       |
  |                                    |
  v                                    |
EVIDENCE MANAGER ----------------------+
  |
  +-- still insufficient
  |       -> INSUFFICIENT_EVIDENCE
  |
  v
CONTEXT BUILDER
  |
  v
LLM
  |
  v
ANSWER VERIFIER
  |
  +-- VALID -> RETURN
  |
  +-- QUESTIONABLE
          |
          v
     bounded repair /
     retrieve / verify
```

------------------------------------------------------------------------

# 4. Source State and Reproducibility

A Git commit alone does not identify the source used to generate
evidence.

Introduce an explicit `SourceRevision`.

``` text
SourceRevision
  id
  repository_id
  commit_sha
  branch
  working_tree_manifest_hash
  discovery_policy_hash
  captured_at
```

This represents:

``` text
HEAD
+
uncommitted working-tree state
+
effective discovery policy
```

A workspace snapshot is a collection of repository source revisions.

``` text
WorkspaceSnapshot
  id
  created_at
  source_revision_ids[]
```

Every evidence object must ultimately answer:

> Exactly which source state produced this evidence?

Required provenance includes:

``` text
repository_id
path
content_hash
source_revision_id
index_generation_id
source_span
```

------------------------------------------------------------------------

# 5. Index Generations and Version Compatibility

Source state and derived-index state are separate.

``` text
IndexGeneration
  id
  source_revision_id
  schema_version
  parser_registry_version
  analyzer_versions
  segmentation_version
  indexer_version
  configuration_hash
  ranking_version
  embedding_model_version
  created_at
```

Version changes have scoped invalidation.

Examples:

``` text
embedding model changed
  -> semantic representations only

segmentation changed
  -> retrieval units + dependent semantic artifacts

language analyzer changed
  -> affected structural/symbol/relationship artifacts

ranking algorithm changed
  -> normally no source reindex

schema changed
  -> migrate or rebuild according to compatibility rules
```

The binary must explicitly determine whether an existing index
generation is:

``` text
COMPATIBLE
MIGRATABLE
PARTIALLY_REBUILDABLE
INCOMPATIBLE
```

Do not perform full workspace rebuilds unnecessarily.

------------------------------------------------------------------------

# 6. Stable Identity Model

Database row IDs are not semantic identities.

Separate:

``` text
Physical identity
Logical identity
Occurrence identity
```

## File identity

``` text
FileIdentity
  repository_id
  stable_identity
```

## File occurrence

``` text
FileOccurrence
  id
  file_identity_id
  source_revision_id
  path
  content_hash
  size
  source metadata
```

## Symbol identity

``` text
SymbolIdentity
  id
  repository_id
  language
  qualified_name
  kind
  disambiguator
```

## Symbol occurrence

``` text
SymbolOccurrence
  id
  symbol_identity_id
  file_occurrence_id
  source_revision_id
  source_span
  signature
```

Identity design must support:

-   file rename
-   function/class movement
-   symbol rename where detectable
-   repository re-cloning
-   branch changes
-   parser changes
-   re-segmentation

Rename/move detection may use:

-   Git rename information
-   content similarity
-   structural similarity
-   logical symbol identity

Identity matching may have confidence and must not silently claim
uncertain matches as exact.

------------------------------------------------------------------------

# 7. Central Storage Architecture

Use one central workspace SQLite database for V1.

``` text
workspace/
  .mcp/
    index.db
    vectors/
    artifacts/
    cache/
    checkpoints/
    state/
    logs/
```

The central DB is an intelligence/state store, not a second full copy of
every repository.

## SQLite stores

``` text
workspace metadata
repositories
source revisions
workspace snapshots
index generations
file metadata
structural nodes
retrieval-unit metadata
symbols
relationships
knowledge metadata
FTS5 searchable content
freshness state
invalidation state
canonical evidence metadata
operational/task state
```

## Repository filesystem / artifact storage stores

``` text
authoritative source
large source artifacts when needed
large derived artifacts where SQLite is inefficient
```

## Optional vector storage stores

``` text
semantic representations
```

Avoid unnecessary duplication of large `retrieval_text` or source blobs
across multiple tables.

Use source spans/references whenever exact content can safely be
retrieved from the authoritative repository.

------------------------------------------------------------------------

# 8. SQLite Concurrency Model

SQLite V1 uses WAL mode.

Architecture:

``` text
Query Workers
    |
    v
concurrent SQLite readers


Analyzer / Index Workers
    |
    v
produce mutations
    |
    v
bounded write queue
    |
    v
DB Writer / Transaction Coordinator
    |
    v
SQLite
```

Do not allow arbitrary numbers of indexing workers to independently
create write contention.

Parsing and analysis remain parallel.

Canonical index state and operational state must be conceptually
separated, for example:

``` text
core_* tables
ops_* tables
```

Recovery must be able to rebuild/discard operational state without
corrupting canonical evidence.

------------------------------------------------------------------------

# 9. Repository-Level Isolation

The central DB does not mean global rebuilds.

Every relevant entity contains `repository_id`.

``` text
Repository
   |
Repository Index State
   |
Workspace Catalog
   |
Cross-Repository Relationships
```

If repo 17 changes:

``` text
repo-17
   |
changed files
   |
affected artifacts invalidated
   |
repo-17 incremental recomputation
   |
affected cross-repository edges updated
```

Other repositories remain untouched unless dependency invalidation
requires targeted updates.

Physical database-per-repository architecture is not required for V1 and
should only be introduced if benchmarks justify it.

------------------------------------------------------------------------

# 10. File Discovery and Ignore Policy

Discovery happens before expensive parsing/indexing.

``` text
Repository
   |
Security Boundary
   |
Load Discovery Policy
   |
   +-- .gitignore
   +-- nested .gitignore
   +-- .git/info/exclude
   +-- MCP workspace rules
   +-- MCP repository rules
   +-- default exclusions
   |
   v
Eligible Files
```

For Git repositories, prefer Git-aware discovery.

Support:

-   tracked files
-   eligible untracked files
-   Git ignore semantics
-   negation rules
-   nested ignore rules
-   explicit MCP include/exclude rules

Typical default exclusions:

``` text
.git/
node_modules/
coverage/
.cache/
__pycache__/
.venv/
venv/
.next/
.gradle/
target/
build/
dist/
```

Do not blindly exclude potentially useful directories such as:

``` text
vendor/
generated/
fixtures/
snapshots/
```

Their policy is configurable.

Discovery classes:

``` text
IGNORED
LOW_PRIORITY
NORMAL
HIGH_PRIORITY
```

Typical defaults:

``` text
node_modules/          IGNORED
.git/                  IGNORED
compiled build output  IGNORED
coverage output        IGNORED

generated source       LOW_PRIORITY/configurable
fixtures               LOW_PRIORITY
snapshots              LOW_PRIORITY

tests                  NORMAL
docs                   NORMAL
config                 NORMAL

src                    HIGH_PRIORITY
lib                    HIGH_PRIORITY
app                    HIGH_PRIORITY
services               HIGH_PRIORITY
knowledge              HIGH_PRIORITY
```

Important:

``` text
indexing priority
!=
retrieval ranking
!=
semantic enrichment priority
```

Tests are normal searchable behavioral evidence but need not dominate
ordinary retrieval or semantic enrichment.

Changes to ignore/discovery rules trigger targeted rediscovery and
invalidation.

------------------------------------------------------------------------

# 11. File Lifecycle

Deletion and exclusion are explicit states.

``` text
FileRecord
  id
  repository_id
  file_identity_id
  path
  content_hash
  size
  language
  file_type
  discovery_class
  security_state
  existence_state
  freshness_state
  last_seen_source_revision
```

Possible states include:

``` text
ACTIVE
DELETED
EXCLUDED
INACCESSIBLE
TOO_LARGE
BINARY
SECRET_REDACTED
PARSER_FAILED
```

A disappeared file must invalidate stale symbols/relationships/evidence
rather than leaving ghost evidence in retrieval.

------------------------------------------------------------------------

# 12. Language-Agnostic Analyzer Architecture

The architecture is not limited to Java, Python, Go, JavaScript, or
TypeScript.

Those are initial high-priority implementations.

Use an `AnalyzerRegistry`.

``` text
Analyzer Registry
   |
   +-- Source Language Analyzers
   |
   +-- Structured Data Analyzers
   |
   +-- Document Analyzers
   |
   +-- Infrastructure / Build Analyzers
   |
   +-- Generic Analyzer
```

Each analyzer advertises capabilities.

``` text
AnalyzerCapabilities
  lexical
  structural_parse
  symbol_extraction
  import_extraction
  reference_extraction
  relationship_resolution
  build_resolution
  semantic_resolution
```

Capability levels are graded.

``` text
Level 0
  generic lexical/search support

Level 1
  structural parsing

Level 2
  symbols/imports

Level 3
  references/resolution

Level 4
  package/build awareness

Level 5
  framework/domain-specific intelligence
```

A language can therefore be useful without having full semantic
resolution.

------------------------------------------------------------------------

# 13. Initial Source-Language Support

Initial Tier-1 analyzers:

``` text
Java
Python
Go
JavaScript
TypeScript
```

Tree-sitter is the preferred structural parsing foundation where
suitable.

Additional languages can be registered without architecture changes,
including for example:

``` text
C
C++
C#
Rust
Kotlin
Scala
Swift
Ruby
PHP
Bash
and others
```

The actual implementation order should follow workspace usage and
benchmark value.

Parser availability must never determine whether a file is searchable.

------------------------------------------------------------------------

# 14. Structured, Document, and Infrastructure Formats

Do not lump every non-programming format into "configuration languages."

Use capability-specific categories.

## Structured data/configuration

Examples:

``` text
JSON
YAML
XML
TOML
INI
.properties
CSV
```

Possible capabilities:

``` text
hierarchy
key/path extraction
schema-aware segmentation
reference extraction
```

## Documents

Examples:

``` text
Markdown
AsciiDoc
reStructuredText
```

Capabilities:

``` text
heading hierarchy
sections
links
code blocks
document metadata
```

## Query/schema/interface languages

Examples:

``` text
SQL
GraphQL
Protocol Buffers
OpenAPI
```

These may have richer analyzers for:

``` text
statements
tables
columns
schemas
operations
types
references
```

## Infrastructure/build formats

Examples:

``` text
Terraform/HCL
Dockerfile
Maven
Gradle
package manifests
CI configuration
Kubernetes manifests
```

These may participate directly in dependency and cross-repository
reasoning.

## Unknown/custom formats

Fallback:

``` text
encoding/type detection
   |
generic structural heuristics
   |
hierarchical segmentation
   |
FTS
```

Unknown content remains searchable.

------------------------------------------------------------------------

# 15. Universal File Pipeline

``` text
File
 |
 +-- canonical path/security validation
 +-- discovery classification
 +-- encoding detection
 +-- type detection
 +-- binary detection
 +-- generated-file classification
 +-- secret processing
 +-- size analysis
 +-- analyzer selection
```

Then:

``` text
specialized analyzer succeeds
    -> rich canonical representation

specialized analyzer fails
    -> generic analyzer

no specialized analyzer
    -> generic analyzer
```

Parser/analyzer failure is observable but never silently removes
eligible text content from search.

------------------------------------------------------------------------

# 16. Large-File Architecture

Large files must not automatically be fully loaded and converted into
giant ASTs.

Use size-aware processing.

``` text
metadata scan
    |
coarse/streaming structural scan
    |
region map
    |
selected deeper parsing
    |
hierarchical retrieval units
```

Example:

``` text
very large XML
   |
streaming element scan
   |
element/region offsets
   |
relevant regions parsed deeply on demand
```

Possible file policies:

``` text
NORMAL
HIERARCHICAL_LARGE_FILE
METADATA_ONLY
IGNORED
```

Processing must enforce:

-   maximum memory budget
-   maximum parser time
-   recursion/depth limits
-   streaming where supported
-   cancellation

The system should remain safe if a repository unexpectedly contains
files much larger than the expected \~4 MB cases.

------------------------------------------------------------------------

# 17. Canonical Structural Model

## Structural node

``` text
StructuralNode
  id
  repository_id
  file_occurrence_id
  parent_id
  node_type
  structural_identity
  source_span
  content_hash
  analyzer_id
  analyzer_version
  metadata
```

## Retrieval unit

``` text
RetrievalUnit
  id
  repository_id
  file_occurrence_id
  retrieval_text_ref/content
  lexical_state
  semantic_state
  freshness_state
```

Do not store `structural_node_ids[]` as a relational array.

Normalize:

``` text
RetrievalUnitNode
  retrieval_unit_id
  structural_node_id
  ordinal
```

This supports:

-   reverse lookup
-   invalidation
-   parent/child analysis
-   symbol-to-unit mapping

Remember:

``` text
structural unit
!=
retrieval unit
!=
context unit
```

------------------------------------------------------------------------

# 18. Relationship Model

``` text
Relationship
  id
  source_repository_id
  source_entity_id
  target_repository_id
  target_entity_id
  type
  dependency_basis
  resolution
  confidence
  provenance
  source_revision_id
  freshness_state
```

Possible dependency basis:

``` text
Maven dependency
Gradle dependency
Go module
npm package
Python package
Git submodule
generated API
configuration reference
import
runtime/framework wiring
heuristic inference
```

Resolution should be richer than a simple resolved/unresolved flag.

Examples:

``` text
syntactic
package_resolved
symbol_resolved
build_resolved
framework_resolved
inferred
```

Graph traversal is evidence expansion, not truth.

Every traversal has budgets:

``` text
max_depth
max_nodes
max_edges
max_tokens
max_time
```

------------------------------------------------------------------------

# 19. Knowledge Model

`knowledge/*.md` is first-class evidence but not automatically
authoritative.

``` text
KnowledgeItem
  id
  repository_scope
  source
  authority
  last_verified
  applicable_versions
  supersedes
  confidence
  content_hash
  freshness_state
```

Knowledge and source code use the same evidence abstraction later.

Authority examples:

``` text
source code
  -> implementation

test
  -> behavioral expectation

knowledge
  -> project documentation

configuration
  -> configured behavior

relationship
  -> derived structural evidence
```

The system must be able to surface contradictions rather than silently
choosing one source.

------------------------------------------------------------------------

# 20. Invalidation Dependency DAG

Invalidation is not recomputation.

Represent dependencies explicitly.

``` text
SourceArtifact
     |
     | derived_from
     v
DerivedArtifact
```

Example:

``` text
FileOccurrence
   |
   +-- StructuralNode
   |      |
   |      +-- SymbolOccurrence
   |      +-- Relationship
   |
   +-- RetrievalUnit
   |      |
   |      +-- SemanticRepresentation
   |
   +-- file/document aggregates
```

When source changes:

``` text
detect change
   |
mark dependent artifacts
STALE / INVALID
   |
scheduler determines
what must be recomputed
```

This improves:

-   incremental indexing
-   crash recovery
-   cancellation
-   prioritization
-   semantic rebuilds
-   version upgrades

------------------------------------------------------------------------

# 21. Freshness Model

Freshness is explicit metadata.

Possible states:

``` text
CURRENT
STALE
UNKNOWN
INVALID
PENDING_REFRESH
```

Track freshness for:

``` text
files
structural artifacts
symbols
relationships
knowledge
semantic artifacts
workspace/index generation
```

V1 does not require a separate large Freshness Manager service.

Freshness is enforced by the Evidence Manager and may later be extracted
into a dedicated service if operational complexity justifies it.

------------------------------------------------------------------------

# 22. Canonical Evidence Object

Evidence is a first-class canonical object.

``` text
Evidence
  id
  repository_id

  source_type
    source_code
    test
    configuration
    documentation
    knowledge
    relationship
    generated_source

  source_id
  path

  source_revision_id
  index_generation_id
  source_span
  content_hash

  freshness
  authority
  confidence
  relationship_confidence

  retrieval_sources[]
  ranking_signals{}

  verification_state
```

All retrievers ultimately produce evidence candidates.

``` text
SourceCode --------+
Test --------------+
Configuration -----+
Knowledge ---------+--> Evidence
Documentation -----+
Graph Relationship +
```

Pipeline:

``` text
retrievers
   |
Evidence candidates
   |
Evidence Ranking
   |
Evidence Validation
   |
ValidatedEvidence[]
   |
Context Builder
```

------------------------------------------------------------------------

# 23. Query Router and Evidence Contract

The Query Router determines query type.

Examples:

``` text
definition lookup
symbol navigation
configuration lookup
architecture explanation
debugging/root cause
impact analysis
cross-repository dependency
knowledge question
test behavior
```

Each query gets a `QueryEvidenceContract`.

``` text
QueryEvidenceContract
  query_type
  required_evidence[]
  preferred_evidence[]
  preferred_sources[]
  freshness_requirement
  relationship_confidence_requirement
  repository_scope
  allowed_fallbacks[]
  expansion_budget
```

Example:

``` text
"Where is FooService implemented?"

required:
  definition evidence

preferred:
  symbol occurrence
  implementation span

freshness:
  current
```

Example:

``` text
"Why can replication silently fail?"

required:
  implementation evidence

preferred:
  callers
  configuration
  tests
  project knowledge

fallback:
  targeted expansion
  source verification
```

------------------------------------------------------------------------

# 24. FAST / NORMAL / DEEP as Resource Policies

Answer modes are explicit policies, not informal hints.

``` text
AnswerModePolicy
  max_time_ms
  max_candidates
  max_graph_depth
  max_graph_nodes
  max_fs_files
  max_fs_bytes
  max_context_tokens
  semantic_allowed
  reranking_allowed
  source_verification_level
  repair_attempts
```

## FAST

Designed for exact/simple questions.

Typical:

``` text
FTS
symbol
structural
minimal graph
minimal context
```

Normally:

-   no semantic retrieval
-   no broad graph expansion
-   no expensive filesystem verification

## NORMAL

Default.

Typical:

``` text
FAST
+ retrieval planning
+ knowledge
+ bounded graph
+ evidence ranking
+ targeted expansion
+ source verification when required
```

## DEEP

For architecture, debugging, impact, ambiguity, or explicitly deep
analysis.

May add:

``` text
semantic retrieval
reranking
broader graph expansion
additional source verification
contradiction analysis
larger evidence/context budget
answer repair
```

Actual budgets are benchmark-calibrated per hardware profile.

------------------------------------------------------------------------

# 25. Retrieval Planner and RetrievalPlan

The Retrieval Planner converts:

``` text
query
+ query type
+ answer mode
+ evidence contract
+ repository scope
+ freshness state
+ available indexes
```

into a reproducible `RetrievalPlan`.

``` text
RetrievalPlan
  id
  query_type
  answer_mode
  repositories[]

  lexical_queries[]
  symbol_queries[]
  structural_queries[]
  knowledge_queries[]

  graph_operations[]
  semantic_operations[]

  fallback_policy
  evidence_requirements
  budgets
```

Log:

``` text
Question
  ->
RetrievalPlan
  ->
Candidates
  ->
Evidence
  ->
Context
  ->
Answer
```

This is central to retrieval debugging and evaluation.

------------------------------------------------------------------------

# 26. Candidate Generation and Fusion

Available candidate generators:

``` text
FTS5 lexical search
exact search
symbol search
structural search
knowledge search
graph search
semantic search
```

Not every query invokes every generator.

Candidate fusion preserves individual signals and retrieval provenance.

It does not decide whether evidence is valid.

------------------------------------------------------------------------

# 27. Evidence Ranking

Ranking answers:

> Which candidates are probably useful?

Signals may include:

``` text
exactness
lexical score
symbol match
query-intent match
repository relevance
freshness
structural proximity
relationship confidence
knowledge authority
test relevance
semantic score
```

Do not hide everything behind an opaque irreversible score.

Example:

``` text
Foo.java
  lexical          0.91
  symbol           1.00
  freshness        1.00
  graph            0.82
  semantic         0.71
  test_relevance   0.20
```

A combined ranking score may exist operationally, but component signals
remain observable.

------------------------------------------------------------------------

# 28. Evidence Validation and Evidence Manager

Ranking and validation are separate.

``` text
Candidate Generation
       |
Candidate Fusion
       |
Ranking
       |
Evidence Validation
       |
Evidence Selection
```

Validation answers:

> Can this candidate actually support the required evidence?

The Evidence Manager checks:

-   provenance
-   source revision
-   index generation
-   freshness
-   authority
-   relationship confidence
-   contradictions
-   evidence-contract satisfaction
-   evidence completeness

A highly ranked stale result still fails freshness requirements.

------------------------------------------------------------------------

# 29. Explicit Insufficient-Evidence State

The system must be allowed to return:

``` text
INSUFFICIENT_EVIDENCE
```

Flow:

``` text
initial evidence
   |
Evidence Manager
   |
sufficient?
  /    \
yes     no
 |       |
 |    targeted expansion
 |       |
 |    Evidence Manager
 |       |
 |    sufficient?
 |      /   \
 |    yes    no
 |     |      |
 |     |   source verification
 |     |      |
 |     |   Evidence Manager
 |     |      |
 |     |   still insufficient
 |     |      |
 |     |   INSUFFICIENT_EVIDENCE
 |     |
 +-----+
   |
context
```

Weak evidence must never force a confident answer.

------------------------------------------------------------------------

# 30. Direct Source Verification

The index proposes evidence; source can verify it.

Conceptual tiers:

``` text
L1 Indexed evidence
       |
L2 Structural / relationship evidence
       |
L3 Direct source verification
```

Direct verification reads the exact current file/region from the
authoritative repository.

Use it when required by:

-   dirty working tree
-   stale/unknown freshness
-   important current-behavior claims
-   contradictions
-   evidence contract
-   insufficient indexed evidence
-   DEEP verification policy

It is not mandatory for every NORMAL query.

It is bounded by:

``` text
max_files
max_total_bytes
max_read_time
repository_scope
allowed_paths
```

It must never become an unrestricted recursive workspace scan.

------------------------------------------------------------------------

# 31. Context Builder

Only validated evidence enters final context.

Responsibilities:

-   exact source spans
-   repository/path provenance
-   source revision
-   index generation
-   useful structural parents
-   relevant imports/signatures
-   configuration
-   tests
-   knowledge
-   contradictions
-   relationship confidence
-   token budgeting
-   deduplication
-   evidence prioritization

The context builder may request a bounded retrieval adjustment if
required evidence cannot fit coherently within the context budget.

------------------------------------------------------------------------

# 32. Answer Verification

V1 answer verification is deterministic-first.

Represent important answer claims:

``` text
Claim
  id
  text
  claim_type
  confidence
  evidence_ids[]
```

Example:

``` text
Claim:
"FooService retries three times."

Evidence:
FooService.java:123-145

Result:
SUPPORTED
```

Versus:

``` text
Claim:
"BillingService calls FooService."

Evidence:
FooService implementation only

Result:
UNSUPPORTED
```

Verify:

-   important claims map to evidence
-   cited spans actually support claims
-   evidence belongs to the expected source revision/index generation
-   freshness has not changed during the query
-   relationship claims satisfy confidence requirements
-   contradictions are surfaced
-   unsupported claims are removed/repaired

A second LLM verifier is not mandatory.

NORMAL/DEEP may use a bounded repair loop:

``` text
QUESTIONABLE
   |
retrieve/verify more
   |
regenerate if needed
```

Repair attempts/time/tokens are strictly bounded.

------------------------------------------------------------------------

# 33. Semantic Intelligence

Approximately 500K lexical/structural retrieval units do not imply 500K
embeddings.

``` text
~500K cheap searchable units
          |
semantic-unit selection
          |
smaller semantic corpus
          |
embeddings
```

Prefer semantically useful units such as:

-   modules
-   files
-   classes
-   important methods/functions
-   documentation sections
-   knowledge sections
-   configuration groups
-   useful aggregates

Semantic representations are disposable derived artifacts.

``` text
source/index remains valid
       |
embedding model changes
       |
discard semantic layer
       |
rebuild semantic representations
```

Do not make semantic correctness a dependency of the canonical
source/structural index.

Semantic enrichment priority:

``` text
P0 user-requested
P1 recently modified
P2 frequently retrieved
P3 highly connected
P4 important repositories
P5 normal source
P6 low-value/generated
P7 rare content
```

Tests may remain normal indexed evidence while receiving selective
semantic enrichment.

------------------------------------------------------------------------

# 34. Security Guarantees

## Workspace allowlist

Only configured roots are accessible.

## Canonical path validation

Every path is canonicalized and checked.

## Symlink protection

Repository symlinks cannot escape allowed roots.

## Repository content is untrusted

Source code, comments, docs, generated content, tests, and knowledge are
evidence---not instructions.

They cannot:

-   modify MCP policy
-   expand filesystem permissions
-   trigger arbitrary commands
-   request secrets
-   override security configuration

## Secret handling

Secret states are explicit:

``` text
FORBIDDEN
SECRET_REDACTED
PARTIALLY_REDACTED
SAFE
```

Detected secret material must never enter:

``` text
FTS
embeddings
summaries
logs
telemetry
answer context
retrieval caches
```

Where safe partial indexing is possible, only redacted content may enter
derived systems.

## Resource protection

Bound:

-   memory
-   CPU
-   disk I/O
-   parser time
-   recursion
-   graph traversal
-   filesystem verification
-   semantic workers
-   repair loops

------------------------------------------------------------------------

# 35. Task and Resource Model

Every expensive operation is a task.

``` text
Task
  id
  repository_id
  type
  priority
  state
  memory_budget
  cpu_budget
  timeout
  cancellation_token
  checkpoint
  retry_policy
```

Phase 1 supports:

-   bounded queues
-   concurrency limits
-   cancellation
-   graceful shutdown
-   retries
-   checkpoints
-   basic memory constraints

Later production optimization adds:

-   isolated heavy workers
-   adaptive concurrency
-   optional GPU budgeting
-   disk-I/O controls
-   memory-pressure pausing
-   idle-time enrichment

Heavy-process isolation is used only where its overhead is justified.

------------------------------------------------------------------------

# 36. MCP Tool Surface

Keep the external interface small.

## `context`

Primary answer-building tool.

``` text
context(question, scope?, mode?)
```

Internally handles:

-   routing
-   evidence contract
-   retrieval planning
-   retrieval
-   ranking
-   validation
-   graph expansion
-   semantic retrieval
-   source verification
-   context construction

## `search`

Explicit discovery/exact search.

## `navigate`

-   callers
-   callees
-   references
-   dependencies
-   dependents

## `impact`

Potential change impact.

## `repo_map`

Workspace/repository structure.

## `file`

Exact file/source-region retrieval.

## `knowledge`

Knowledge retrieval/management.

## `status`

-   source revisions
-   workspace snapshot
-   index generations
-   freshness
-   indexing
-   enrichment
-   resources

## `feedback`

Record retrieval usefulness/corrections.

The agent should not need to understand internal retrieval
implementation details.

------------------------------------------------------------------------

# 37. Observability

Every query should be diagnosable.

Record:

``` text
query id
question
query type
answer mode
workspace snapshot
source revisions
index generations
RetrievalPlan
retrieval channels
candidate counts
ranking signals
evidence selected/rejected
freshness
contradictions
source verification usage
graph traversal budget/use
semantic usage
context size
claim/evidence mapping
answer-verification result
repair attempts
latencies
resource usage
```

Indexing telemetry includes:

``` text
files discovered
files ignored
parser/analyzer fallback
invalidations
recomputations
queue depth
write latency
peak RAM
task retries
task cancellation
```

------------------------------------------------------------------------

# 38. Evaluation

Evaluation is layered.

``` text
Retrieval correctness
        |
Evidence correctness
        |
Answer correctness
        |
Answer completeness
        |
Unsupported claims
```

## Retrieval

-   Recall@5
-   Recall@10
-   MRR
-   nDCG

## Evidence

-   evidence precision
-   evidence recall where ground truth exists
-   provenance correctness
-   freshness correctness
-   evidence-contract satisfaction
-   contradiction detection

## Answer

-   correctness
-   completeness
-   groundedness
-   citation/provenance correctness
-   unsupported-claim rate
-   contradiction rate
-   correct no-answer behavior

## Operational

-   initial indexing time
-   incremental indexing time
-   retrieval latency
-   context size
-   peak RAM
-   CPU
-   invalidation size
-   semantic cost
-   SQLite write contention
-   direct-source verification cost

Start with approximately 100--200 representative real engineering
questions.

Expand toward 500--2,000 after the evaluation framework stabilizes.

------------------------------------------------------------------------

# 39. Performance Profiles

Targets are tied to hardware profiles.

## Minimum

``` text
~8 CPU cores
16 GB RAM
CPU-only semantic processing
```

## Typical developer workstation

``` text
modern Apple Silicon or equivalent
32 GB RAM
```

## High-end

``` text
16+ CPU cores
64 GB RAM
optional GPU
```

Candidate targets:

``` text
Fast index:
  usable within minutes

FAST exact/lexical:
  approximately <100–300 ms

NORMAL retrieval:
  approximately 200–1000 ms where possible

DEEP cross-repository:
  approximately 1–3+ seconds depending on work required
```

These are benchmark targets, not guarantees.

LLM generation latency is measured separately.

------------------------------------------------------------------------

# 40. Development Phases

## Phase 0 --- Contracts and Benchmark

Deliver:

-   100--200 representative benchmark questions
-   KG-MCP baseline
-   optional external baseline
-   SourceRevision contract
-   WorkspaceSnapshot contract
-   IndexGeneration contract
-   stable identity rules
-   analyzer capability contract
-   canonical Evidence contract
-   RetrievalPlan contract
-   Query Evidence Contract
-   invalidation DAG contract
-   AnswerModePolicy contract
-   security/secret contract
-   schema/version compatibility rules
-   evaluation metrics

### Gate

No major optimization without a measurable baseline and agreed
implementation contracts.

------------------------------------------------------------------------

## Phase 1 --- Minimum Useful MCP

Deliver:

-   Rust MCP
-   central SQLite + WAL
-   bounded DB writer
-   repository discovery
-   `.gitignore` / `.git/info/exclude`
-   configurable discovery policy
-   source revisions
-   workspace snapshots
-   index generations
-   file lifecycle
-   Analyzer Registry
-   generic analyzer
-   initial Tier-1 source analyzers
-   structured/document analyzers needed by real repositories
-   universal fallback
-   FTS5
-   basic structural model
-   symbols/imports
-   normalized retrieval-unit mapping
-   knowledge ingestion
-   basic Evidence object creation
-   provenance
-   security boundaries
-   secret non-persistence
-   minimal task/resource framework
-   basic `search`, `file`, `repo_map`, `status`

No embeddings required.

### Gate

Useful lexical/structural MCP that can be benchmarked against KG-MCP.

------------------------------------------------------------------------

## Phase 2 --- Incremental Correctness and Freshness

Deliver:

-   filesystem watcher
-   ignored-path filtering
-   event coalescing
-   hash-based detection
-   rename/move handling
-   dependency DAG
-   invalidation without immediate recomputation
-   repository-scoped updates
-   freshness states
-   deletion handling
-   checkpoints
-   crash recovery
-   idempotent tasks
-   stale-state detection
-   source-revision changes during indexing

### Gate

Repository changes update only affected artifacts and no ghost/stale
evidence is treated as current.

------------------------------------------------------------------------

## Phase 3 --- Structural Intelligence

Deliver:

-   deeper Tree-sitter integration
-   capability-based language analyzers
-   symbols
-   references
-   imports/exports
-   package resolution
-   build-system relationships
-   richer relationship resolution
-   cross-file navigation
-   additional languages according to benchmark/workspace demand

### Gate

Measurable improvement on symbol/navigation/dependency questions.

------------------------------------------------------------------------

## Phase 4 --- Evidence-Driven Retrieval

Deliver:

-   Query Router
-   formal FAST/NORMAL/DEEP policies
-   Query Evidence Contracts
-   Retrieval Planner
-   persisted/loggable RetrievalPlan
-   candidate fusion
-   Evidence Ranking
-   Evidence Manager
-   evidence validation
-   evidence completeness
-   contradiction detection
-   bounded graph expansion
-   direct source verification
-   `INSUFFICIENT_EVIDENCE`
-   Context Builder
-   deterministic claim/evidence Answer Verifier
-   bounded repair loop

### Gate

Strong answer quality without requiring semantic embeddings.

------------------------------------------------------------------------

## Phase 5 --- Semantic Intelligence

Introduce sequentially:

1.  semantic-unit selection
2.  embeddings
3.  benchmark
4.  selective enrichment
5.  hybrid retrieval
6.  benchmark
7.  reranking
8.  benchmark
9.  optional summaries

Semantic representations remain disposable.

### Gate

Each semantic capability remains only if it materially improves the
benchmark relative to its operational cost.

------------------------------------------------------------------------

## Phase 6 --- Cross-Repository Intelligence

Deliver:

-   workspace dependency catalog
-   cross-repository package/build relationships
-   cross-repository symbol resolution where feasible
-   graph expansion
-   dependents/dependencies
-   impact analysis
-   cross-repository context construction

### Gate

Strong performance on real cross-repository questions with controlled
graph expansion.

------------------------------------------------------------------------

## Phase 7 --- Production Optimization

Deliver:

-   isolated heavy workers
-   adaptive CPU/RAM usage
-   optional GPU support
-   disk-I/O budgeting
-   advanced scheduling
-   production telemetry
-   Linux/macOS hardening
-   packaging
-   migrations
-   security audit
-   operational documentation

### Gate

Production comparison against KG-MCP and defined
correctness/performance/resource targets.

------------------------------------------------------------------------

# 41. Explicit V1 Non-Goals

Do not block V1 on:

-   embedding all \~500K units
-   LLM summaries for every file
-   every Tree-sitter language
-   perfect semantic call graphs
-   perfect framework resolution
-   distributed databases
-   Elasticsearch
-   Neo4j
-   Redis
-   GPU support
-   sophisticated adaptive scheduling
-   SWE-bench
-   physical DB per repository
-   mandatory second-LLM answer verification

------------------------------------------------------------------------

# 42. Architecture Freeze Rule

After this canonical plan is accepted, freeze the high-level
architecture **unless correctness, security, scalability, benchmark
evidence, or production evidence demonstrates that a structural change
is required**.

The freeze prevents speculative redesign.

It must never prevent necessary corrections.

Implementation details, algorithms, analyzers, ranking models,
thresholds, storage optimizations, and semantic models remain evolvable.

------------------------------------------------------------------------

# 43. Final Engineering Priorities

In order:

1.  measurable benchmark
2.  source-revision correctness
3.  stable logical identities
4.  security and secret non-persistence
5.  Git-aware discovery
6.  central DB with repository isolation
7.  analyzer registry and universal fallback
8.  correct file lifecycle
9.  dependency-aware invalidation DAG
10. freshness
11. canonical Evidence
12. reproducible RetrievalPlan
13. FAST/NORMAL/DEEP resource policies
14. evidence ranking
15. evidence validation
16. explicit insufficient-evidence handling
17. bounded direct-source verification
18. deterministic answer verification
19. structural intelligence
20. retrieval quality
21. selective/disposable semantic intelligence
22. cross-repository intelligence
23. production resource optimization

The system succeeds when it produces **reproducible, current, secure,
evidence-backed engineering answers**.

It does not succeed merely because it has a large index, graph, vector
store, or sophisticated retrieval stack.
