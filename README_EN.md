# PDM Lite (3dPDMCraft)

English | [中文](README.md)

A lightweight CAD data management system: local workspace + Alibaba Cloud OSS sync + part-level version history + multi-device sync + conflict management. Single Rust binary, no central server (metadata lives in OSS as immutable events, reconciled by every device).

## Features

- **Dual-folder workspace**: `native/` (parametric source files: CATPart/sldprt/scdoc…) + `neutral/` (neutral derivatives: STEP/STL/CGR…), grouped into logical "parts" by relative-path stem
- **Part-revision model** (inspired by Siemens Teamcenter Item/Revision/Dataset): BLAKE3 content-addressed objects that are never overwritten — dedup for free, history and rollback with zero re-upload
- **Conflict management without hard locks**: edit-intent reservations (auto-expire in 48h) + base-revision staleness detection + three-option resolution (use mine / use theirs / keep both = fork) — no option ever loses data
- **Neutral (derived) conflicts resolve automatically** (newest-wins); only native conflicts require a human
- **Single-part / assembly-tree fork**: zero-copy; assembly forks follow the STEP dependency graph
- **Assembly dependency graph**: parses PRODUCT entities from neutral STEP text (heuristic), transitive impact queries
- **History retention policy**: protection rules (current / base-revision refs / open conflicts / last N revisions / younger than T days / pinned) + dry-run first + gc event broadcast
- **Multi-workspace**: several local directories per machine, each bound to its own remote (bucket/prefix may differ); the second device's init adopts the existing remote history (clone semantics)
- **Web UI + full CLI**

## Build

```bash
cargo build --release   # output: target/release/pdm(.exe)
```

## Quick start (Web UI)

```bash
pdm serve --port 8420    # open http://127.0.0.1:8420
```

The UI covers: add/init workspaces (with OSS connection test) → files/history/rollback → conflict queue → dependency graph/impact → prune.

## CLI

Everything the Web UI does is available on the command line (safe to run alongside `pdm serve`; SQLite WAL serializes access):

| Command | Description |
|---|---|
| `pdm init <dir> --endpoint E --bucket B --ak K --sk S [--prefix P] [--backend oss\|local] [--store-root <shared dir>] [--user U]` | Initialize a workspace; `local` backend works offline, `--store-root` pointing at a shared directory simulates two machines |
| `pdm serve [--port 8420]` | Daemon: file watcher + 60s pull loop + Web UI |
| `pdm workspaces [--json]` | List registered workspaces |
| `pdm status <ws> [--json]` | Part/file/revision/object statistics |
| `pdm files <ws> [--json]` | File table (with remote-ahead / pending badges) |
| `pdm history <ws> --part <part> [--json]` | Revision history |
| `pdm pull <ws>` | Pull remote events immediately |
| `pdm rescan <ws>` | Detect local changes + rebuild dependency graph (**the local-change entry point when `serve` is not running**) |
| `pdm rollback <ws> --part <part> --seq <n>` | Rollback (pointer move + workspace rewrite) |
| `pdm conflicts <ws> [--json]` | Open conflict queue |
| `pdm resolve <ws> --id <id> --mine\|--theirs\|--fork [--fork-name <name>]` | Resolve a conflict (never deletes objects) |
| `pdm fork <ws> --part <part> --new-name <name> [--seq <n>]` | Zero-copy single-part fork |
| `pdm fork-tree <ws> --part <assembly> [--suffix <suffix>]` | Assembly-tree fork (all transitive dependencies) |
| `pdm prune <ws> [--execute]` | History pruning (dry-run by default) |
| `pdm deps <ws> [--json]` / `pdm impact <ws> --part <part>` | Dependency edges / impacted assemblies |
| `pdm remove <ws>` | Unregister a workspace (keeps all data) |
| `pdm test-oss --endpoint E --bucket B --ak K --sk S [--prefix P]` | Probe-test the OSS connection |

## Configuration (`<workspace>/.pdm/config.toml`)

OSS connection, `prune_n` (keep last N revisions per part, default 10), `prune_t_days` (default 90),
`auto_prune` / `auto_prune_hours` (scheduled pruning, default off / 24h), `user`.

Deletions and ignore rules (`~$*`, `*.sld~`, …) live in `<workspace>/.pdm/ignore.conf`.

## Design & operations

- Full design document (conflict management / sync protocol / data model / retention): [docs/design.md](docs/design.md) (Chinese)
- OSS lifecycle rules (console-side ops config): transition `objects/` prefix to IA after 180 days / archive after 365 days; keep `meta/` on standard storage
- Credential hygiene: use a minimal RAM sub-account (read/write/delete on one bucket + prefix only); never reuse the master AK
