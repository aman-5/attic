# Resource Enforcement Contract

**Contract ID:** C14  
**Version:** 1.0.0  
**Status:** DRAFT  
**Depends on:** C12 (answer_modes), C13 (retrieval_plan)

---

## 1. Purpose

This contract defines the **resource enforcement model** for all long-running and background tasks in the Attic server. It covers:

1. The `Task` abstraction — a unit of cancellable, budget-bounded work.
2. How `AnswerModePolicy` budgets are translated into `ResourceBudget` for sub-tasks.
3. CPU, memory, I/O, and time limits for indexing, analysis, and retrieval operations.
4. Backpressure and admission control for the bounded DB writer queue.
5. Resource accounting for the workspace watcher and incremental update pipeline.

Key design decisions:
- **No unbounded allocations.** Every task that enters the system declares its resource class upfront.
- **Cooperative cancellation via `CancellationToken`.** All async tasks check the token at yield points; no force-kill.
- **Resource budgets are per-task, not per-request.** A single MCP query may spawn multiple tasks; each has its own budget derived from the parent policy.
- **Starvation prevention.** Background indexing tasks MUST yield CPU to retrieval tasks within 50 ms.

---

## 2. Task Struct

```
Task {
    task_id:           Uuid,
    task_class:        TaskClass,
    parent_query_id:   Option<Uuid>,        // None for background tasks
    budget:            ResourceBudget,
    cancellation:      CancellationToken,
    created_at_us:     i64,
    deadline_us:       Option<i64>,         // derived from budget.max_time_ms
    status:            TaskStatus,
}
```

### 2.1 TaskClass

```
enum TaskClass {
    // Retrieval pipeline (foreground; latency-sensitive)
    QUERY_RETRIEVAL,        // top-level query; spawns sub-tasks
    GRAPH_WALK,             // dependency graph traversal
    SOURCE_VERIFICATION,    // filesystem re-read for evidence freshness
    CONTEXT_ASSEMBLY,       // assembling final context window

    // Indexing pipeline (background; throughput-sensitive)
    FULL_REINDEX,           // triggered on first discovery or incompatible migration
    INCREMENTAL_UPDATE,     // triggered by file watcher events
    ANALYZER_RUN,           // single-file analyzer execution
    EMBEDDING_GENERATION,   // vector embedding for a batch of retrieval units
    SECRET_SCAN,            // secret scanning pass

    // Maintenance (low-priority; best-effort)
    STALE_EVICTION,         // removing STALE artifacts past TTL
    LOG_PRUNING,            // removing ops_* records past retention window
    INTEGRITY_CHECK,        // verifying stored hashes against filesystem
}
```

### 2.2 TaskStatus

```
enum TaskStatus {
    QUEUED,
    RUNNING,
    COMPLETED,
    CANCELLED { reason: CancellationReason },
    FAILED { message: String },
}

enum CancellationReason {
    DEADLINE_EXCEEDED,
    PARENT_CANCELLED,
    SHUTDOWN_REQUESTED,
    RESOURCE_BUDGET_EXHAUSTED,
    EXPLICIT_ABORT,
}
```

---

## 3. ResourceBudget Struct

```
ResourceBudget {
    max_time_ms:          u64,       // wall-clock deadline from task start
    max_memory_bytes:     u64,       // heap allocation ceiling for this task
    max_cpu_time_ms:      u64,       // CPU time (sum across threads) ceiling
    max_open_files:       u32,       // max simultaneous open file handles
    max_read_bytes:       u64,       // total bytes read from disk
    max_db_read_rows:     u64,       // total rows read from SQLite
    max_db_write_rows:    u64,       // total rows written to SQLite
    max_spawned_tasks:    u8,        // max child tasks this task may spawn
    priority:             TaskPriority,
}

enum TaskPriority {
    CRITICAL,    // query retrieval; never preempted
    HIGH,        // incremental updates triggered by active file edits
    NORMAL,      // standard background indexing
    LOW,         // maintenance tasks
}
```

### 3.1 V1 Default Resource Budgets by TaskClass

| TaskClass | max_time_ms | max_memory_bytes | max_cpu_ms | max_open_files | max_read_bytes | max_db_read_rows | max_db_write_rows | max_spawned_tasks | priority |
|-----------|-------------|-----------------|------------|---------------|----------------|-----------------|------------------|------------------|----------|
| QUERY_RETRIEVAL (FAST) | 300 | 64 MB | 250 | 0 | 0 | 10 000 | 50 | 4 | CRITICAL |
| QUERY_RETRIEVAL (NORMAL) | 3 000 | 256 MB | 2 500 | 20 | 4 MB | 100 000 | 100 | 8 | CRITICAL |
| QUERY_RETRIEVAL (DEEP) | 30 000 | 512 MB | 25 000 | 200 | 100 MB | 1 000 000 | 200 | 16 | CRITICAL |
| GRAPH_WALK | 2 000 | 32 MB | 1 500 | 0 | 0 | 50 000 | 0 | 0 | CRITICAL |
| SOURCE_VERIFICATION | 5 000 | 16 MB | 4 000 | 50 | 20 MB | 1 000 | 0 | 0 | CRITICAL |
| CONTEXT_ASSEMBLY | 1 000 | 128 MB | 800 | 0 | 0 | 500 | 0 | 0 | CRITICAL |
| FULL_REINDEX | 3 600 000 | 512 MB | unlimited | 64 | unlimited | unlimited | unlimited | 8 | NORMAL |
| INCREMENTAL_UPDATE | 30 000 | 128 MB | 25 000 | 16 | 50 MB | 10 000 | 5 000 | 4 | HIGH |
| ANALYZER_RUN | 10 000 | 64 MB | 8 000 | 4 | 4 MB | 100 | 500 | 0 | NORMAL |
| EMBEDDING_GENERATION | 60 000 | 256 MB | 50 000 | 0 | 0 | 1 000 | 1 000 | 0 | NORMAL |
| SECRET_SCAN | 5 000 | 32 MB | 4 000 | 4 | 4 MB | 0 | 100 | 0 | HIGH |
| STALE_EVICTION | 10 000 | 16 MB | 5 000 | 0 | 0 | 10 000 | 10 000 | 0 | LOW |
| LOG_PRUNING | 5 000 | 8 MB | 3 000 | 0 | 0 | 100 000 | 100 000 | 0 | LOW |
| INTEGRITY_CHECK | 300 000 | 64 MB | 200 000 | 64 | 500 MB | 100 000 | 100 | 0 | LOW |

> `unlimited` entries have no enforced ceiling in V1; they rely on OS-level limits.

---

## 4. Admission Control

### 4.1 Concurrency Limits

```
RULE RC-A1: V1 Global Concurrency Limits (configurable at startup)

  max_concurrent_query_tasks:       16   // CRITICAL priority
  max_concurrent_indexing_tasks:    4    // NORMAL/HIGH priority background
  max_concurrent_maintenance_tasks: 2    // LOW priority

  These are soft limits enforced by the task scheduler.
  If the limit is reached for a class, new tasks of that class are QUEUED.
  CRITICAL tasks are never queued; they preempt LOW priority tasks if necessary.
```

### 4.2 DB Writer Queue

```
RULE RC-A2: The DB writer queue has a bounded capacity:
  max_db_writer_queue_depth: 1024   // pending write transactions

  IF queue_depth >= max_db_writer_queue_depth THEN
    new write requests are REJECTED with BackpressureError
    the caller (indexing task) MUST apply exponential backoff:
      base_delay_ms = 10
      max_delay_ms  = 5000
      max_retries   = 20
    If all retries are exhausted, the task transitions to FAILED.
  END

RULE RC-A3: Write transactions from CRITICAL priority tasks bypass the
  backpressure queue and are written immediately. This ensures retrieval
  plan persistence never blocks on indexing backlog.
```

### 4.3 Memory Pressure

```
RULE RC-A4:
  IF total_heap_bytes > 80% of process memory limit THEN
    pause all NORMAL and LOW priority tasks
    emit MemoryPressureWarning event
    resume when total_heap_bytes < 60% of process memory limit
  END

  IF total_heap_bytes > 95% of process memory limit THEN
    cancel all LOW priority tasks immediately
    cancel all NORMAL priority tasks after their current yield point
    emit MemoryPressureCritical event
  END
```

---

## 5. Cancellation Protocol

### 5.1 Cooperative Cancellation

```
RULE RC-C1:
  Every async task MUST check its CancellationToken at every I/O
  await point and at least every 50 ms of CPU-bound work.

  Pseudo-code pattern:
    loop {
        cancellation.check()?;    // returns Err if cancelled
        do_work_unit();
    }
```

### 5.2 Cascading Cancellation

```
RULE RC-C2:
  When a parent task is cancelled, ALL child tasks MUST be cancelled
  within 100 ms.
  Child tasks that do not complete within 100 ms of receiving
  cancellation signal are logged as CancellationTimeout and their
  resources are force-released.

RULE RC-C3:
  Cancellation MUST NOT leave the database in an inconsistent state.
  Any open write transaction at the time of cancellation MUST be
  rolled back, not committed.
```

### 5.3 Shutdown Protocol

```
RULE RC-C4: Graceful Shutdown Sequence
  1. Stop accepting new MCP tool calls
  2. Send cancellation to all NORMAL and LOW priority tasks
  3. Allow CRITICAL tasks up to 5 seconds to complete
  4. Force-cancel remaining CRITICAL tasks
  5. Flush the DB writer queue (up to 2 seconds)
  6. Close SQLite connection with WAL checkpoint
  7. Exit
```

---

## 6. Resource Accounting

```
RULE RC-R1:
  Resource consumption MUST be tracked incrementally, not estimated.
  Each subsystem reports consumed resources back to the parent task
  after completion:
    consumed.max_read_bytes    += step.bytes_read
    consumed.max_db_read_rows  += step.db_rows_read
    consumed.max_db_write_rows += step.db_rows_written

RULE RC-R2:
  If a sub-task would exceed the parent task's remaining budget,
  the sub-task MUST be spawned with the remaining budget, not the
  default budget for its class.

RULE RC-R3:
  Budget exhaustion is detected before allocation, not after.
  IF remaining_budget.max_read_bytes < next_step_estimated_bytes THEN
    skip the step; record as DEGRADED in the RetrievalPlan
  END
```

---

## 7. Starvation Prevention

```
RULE RC-SP1:
  Background FULL_REINDEX tasks MUST voluntarily yield at each
  file boundary (after processing one file).
  Yield means: release the async executor thread for at least one
  scheduler tick before picking up the next file.

RULE RC-SP2:
  If a CRITICAL priority task is waiting in any shared queue,
  all NORMAL and LOW tasks MUST yield within 50 ms.

RULE RC-SP3:
  ANALYZER_RUN tasks MUST be spawned with a `tokio::task::spawn_blocking`
  wrapper for CPU-bound parse work to prevent blocking the async runtime.
```

---

## 8. Resource Metrics

The server MUST maintain the following counters (exposed via the `attic_status` MCP tool in a future phase):

```
ResourceMetrics {
    active_critical_tasks:     u32,
    active_normal_tasks:       u32,
    active_low_tasks:          u32,
    queued_tasks:              u32,
    db_writer_queue_depth:     u32,
    total_heap_estimate_bytes: u64,
    cancelled_tasks_total:     u64,
    budget_exhausted_total:    u64,
    backpressure_events_total: u64,
}
```

---

## 9. Invariants

| ID       | Invariant |
|----------|-----------|
| RC-INV-1 | Every task MUST be associated with a `ResourceBudget` at creation. No task may run without a budget. |
| RC-INV-2 | A cancelled task MUST rollback any open DB transaction before releasing resources. |
| RC-INV-3 | `max_spawned_tasks = 0` tasks MUST NOT call any task-spawning API. |
| RC-INV-4 | CRITICAL tasks MUST always have `max_time_ms` ≤ the parent `AnswerModePolicy.max_time_ms`. |
| RC-INV-5 | The DB writer queue MUST be drained before the process exits (up to the 2-second timeout). |
| RC-INV-6 | Memory budgets are advisory in V1 (Rust does not have a per-task allocator); violations are logged but do not cause hard cancellation. Hard enforcement is deferred to post-V1. |
| RC-INV-7 | `TaskPriority::CRITICAL` tasks are NEVER placed in the admission control queue. |

---

## 10. Test Matrix

| ID      | Scenario | Expected Outcome |
|---------|----------|-----------------|
| RC-01   | QUERY_RETRIEVAL task exceeds `max_time_ms` | Task cancelled via `CancellationToken`; all children cancelled within 100 ms |
| RC-02   | DB writer queue at capacity; indexing task submits write | `BackpressureError`; exponential backoff applied; task retries |
| RC-03   | CRITICAL task submits write while queue is full | Write bypasses queue; committed immediately |
| RC-04   | Parent task cancelled; 3 child tasks running | All 3 children receive cancellation signal within 100 ms |
| RC-05   | ANALYZER_RUN task for CPU-bound parse work | Spawned via `spawn_blocking`; async runtime thread not blocked |
| RC-06   | FULL_REINDEX task running; QUERY_RETRIEVAL arrives | Reindex yields at next file boundary; query runs at CRITICAL priority |
| RC-07   | Total heap exceeds 80% of process limit | All NORMAL and LOW tasks paused; warning emitted |
| RC-08   | Sub-task would exceed remaining budget of parent | Sub-task spawned with remaining budget, not class default |
| RC-09   | Graceful shutdown initiated while CRITICAL query running | Query given up to 5 seconds; WAL checkpoint on shutdown |
| RC-10   | Task cancelled mid-write transaction | Transaction rolled back; no partial write persisted |

---

## 11. Open Questions

| ID     | Question | Impact |
|--------|----------|--------|
| RC-Q1  | Should memory budgets be hard-enforced in V1 using a custom allocator? | Deferred; advisory only in V1 |
| RC-Q2  | Should per-workspace resource quotas be supported (e.g., limiting one workspace's indexing)? | Deferred to post-V1 |
| RC-Q3  | What is the correct concurrency ceiling for `EMBEDDING_GENERATION` if a GPU is available? | Configuration parameter; V1 defaults to 1 (CPU-only) |
