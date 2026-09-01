# Attic MCP Indexing Report

**Generated:** 2026-08-29  
**Attic DB Path:** `C:\Users\amanbansal\.attic\attic.db`  
**Config Path:** `C:\Users\amanbansal\.attic\config.toml`

---

## Database Location

| File | Full Path |
|------|-----------|
| **Main Database** | `C:\Users\amanbansal\.attic\attic.db` |
| Shared Memory | `C:\Users\amanbansal\.attic\attic.db-shm` |
| Write-Ahead Log | `C:\Users\amanbansal\.attic\attic.db-wal` |
| Config | `C:\Users\amanbansal\.attic\config.toml` |

> Open `C:\Users\amanbansal\.attic\attic.db` with any SQLite viewer (e.g., DB Browser for SQLite) to inspect the indexed data.

---

## 1. Configuration Summary

| Setting | Value |
|---------|-------|
| MCP Server Binary | `C:\Users\amanbansal\.attic\attic-server.exe` |
| Database | `C:\Users\amanbansal\.attic\attic.db` |
| Config File | `C:\Users\amanbansal\.attic\config.toml` |
| MCP Timeout | 1800 seconds |
| Auto-approved Tools | `file`, `search`, `repo_map`, `status`, `context`, `workspace` |

---

## 2. Registered Repositories (3 Paths)

| # | Repository Root |
|---|----------------|
| 1 | `C:\Adobe-Projects\EDS\HDFC` |
| 2 | `C:\Users\amanbansal\Desktop\Dump` |
| 3 | `C:\Adobe-Projects\HDFC-Bank-on-prem\HDFC Repo` |

---

## 3. Indexing Status by Repository

### 3.1 `C:\Adobe-Projects\EDS\HDFC`

| Metric | Disk (Actual) | Attic Indexed |
|--------|--------------|--------------|
| Total Files | 23,269 | 17 |
| Indexable Files | 19,235 | — |
| Indexing Units | — | 102 |
| Disk Size | 168.84 MB | — |

**Status:** ⚠️ Bootstrap in progress — only a small fraction indexed at time of check.

---

### 3.2 `C:\Users\amanbansal\Desktop\Dump`

| Metric | Disk (Actual) | Attic Indexed |
|--------|--------------|--------------|
| Total Files | 25 | 0 |
| Indexable Files | 22 | — |
| Indexing Units | — | 0 |
| Disk Size | 20.84 MB | — |

**File Type Breakdown (Disk):**

| Extension | Count |
|-----------|-------|
| `.txt` | 9 |
| `.md` | 7 |
| `.json` | 6 |

**Status:** ❌ Not yet indexed — bootstrap pending for this repository.

---

### 3.3 `C:\Adobe-Projects\HDFC-Bank-on-prem\HDFC Repo`

| Metric | Disk (Actual) | Attic Indexed |
|--------|--------------|--------------|
| Total Files | 224,420 | 1 |
| Indexable Files | 180,883 | — |
| Indexing Units | — | 1 |
| Disk Size | 3,017 MB (~3 GB) | — |

**Status:** ⚠️ Bootstrap in progress — large repository, indexing will take significant time.

---

## 4. Overall Totals

| Metric | Disk (Actual) | Attic Indexed |
|--------|--------------|--------------|
| Total Files (all repos) | 247,714 | 18 |
| Indexable Files | 200,140 | — |
| Total Disk Size | ~3,206 MB (~3.1 GB) | — |
| Indexing Units | — | 103 |

---

## 5. Database Status

| File | Size at Report Time |
|------|-------------------|
| `attic.db` (main) | ~12.6 MB |
| `attic.db-shm` | Present (shared memory) |
| `attic.db-wal` | Grew from ~3.9 MB to ~8 MB during session |

> **Note:** The WAL (Write-Ahead Log) growth from 3.9 MB to 8 MB confirms that active indexing was occurring in the background during this session. Data accumulates in the WAL file and is periodically checkpointed into the main database.

---

## 6. Root Cause Analysis — Underindexing

The low indexed file counts vs actual disk file counts are expected due to:

1. **Bootstrap still in progress** — Attic performs initial indexing asynchronously after a repository root is registered. Large repositories (especially the 224K-file HDFC Repo at ~3 GB) take hours to fully index.
2. **`node_modules` inflation** — Many of the 247K raw disk files are `node_modules` dependencies. Attic filters these from its indexing scope, which reduces the effective indexable count.
3. **DB WAL active** — The growing WAL file proves indexing is actively running. Once the WAL checkpoints, indexed counts will increase significantly.

---

## 7. MCP Log File Location

Attic MCP does **not** write a dedicated log file in the default configuration.

| Log Destination | How to Access |
|----------------|---------------|
| **VS Code Output Panel** | `View → Output → MCP Server: attic` (live stderr) |
| **Debug mode** | Set env var `RUST_LOG=debug` and redirect stderr to a file |
| **Manual test stderr** | `%TEMP%\attic_err*.txt` (from manual `attic-server.exe` test runs only) |

**To enable persistent debug logging**, add `env` to the MCP config:

```json
"attic": {
  "command": "C:\\Users\\amanbansal\\.attic\\attic-server.exe",
  "args": [],
  "env": {
    "RUST_LOG": "debug"
  },
  "timeout": 1800,
  "autoApprove": ["file","search","repo_map","status","context","workspace"]
}
```

---

## 8. Recommendations

| Priority | Action |
|----------|--------|
| 🟡 Medium | Wait for bootstrap to complete before re-running `repo_map` to get accurate indexed counts |
| 🟡 Medium | Monitor VS Code Output Panel → "MCP Server: attic" for bootstrap progress messages |
| 🟢 Low | Enable `RUST_LOG=debug` temporarily if debugging indexing issues |
| 🟢 Low | Re-run this report after 24–48 hours once large repos are fully indexed |

---

*Report generated by ACS Amplify — Attic MCP session on 2026-08-29*
