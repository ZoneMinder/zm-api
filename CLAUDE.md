# Agent Context (CLAUDE)

This file is high-signal context for coding agents (Claude Code) working in this repository.
Keep it practical: commands that work, where code lives, and project-specific conventions.

## Project Summary

`zm-api` is a Rust REST API server for managing a ZoneMinder installation.

- Web framework: Axum v0.8.8
- ORM: SeaORM v1.1 (current app config builds a `mysql://...` URL)
- API docs: Utoipa v5.4.0 + Swagger UI (`/api-docs/openapi.json`, `/swagger-ui`)
- Live streaming is WebRTC + HLS (the earlier MSE scaffolding was removed in commit `ab731fc`). Treat streaming as product code: don’t refactor broadly unless the task requires it.
- Live media comes from zmc’s per-monitor **stream socket** (`{ZM_PATH_SOCKS}/stream_{id}.sock`, one connection carrying video + audio with a HELLO codec handshake — see ZoneMinder’s `docs/stream_socket.rst`). The old media FIFOs are gone; `src/streaming/source/` holds the protocol parser (`protocol.rs`), reader (`stream_socket.rs`), shared media helpers (`media.rs`) and router. The sockets are mode 0660, owned by ZoneMinder’s account; zm-api runs as that same account (`ZM_WEB_USER` from zm.conf, resolved by `packaging/setup-instance.sh` into a systemd drop-in), so no group setup is needed.

## Guardrails (Important)

- Prefer small, targeted changes; don’t “clean up” unrelated code.
- When you need authoritative details on Rust or Axum behavior/APIs (extractors, response types, middleware layers, Tower traits, async semantics), use the Context7 MCP to look it up rather than guessing.
- When you need repository context (PR discussion, review comments, issue history, CI failures), use the GitHub MCP to fetch it rather than guessing.
- Work tests-first: add/adjust tests before (or alongside) implementation; don’t merge code without green tests.
- Keep CI green: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must pass.
- Don’t commit secrets. Local profiles like `settings/dev.toml` are intentionally gitignored.
- Treat `src/entity/` and SeaORM active enums as generated artifacts unless you are intentionally regenerating them.
- Dates and times use `jiff`. `chrono` is not a direct dependency: SeaORM's `DateTime` and `DateTimeUtc` are chrono types, so convert them with `src/util/time.rs` as a value leaves an entity or enters a query, and don't call chrono methods on them anywhere else.
- If you need to change DB schema expectations, update migrations/tests/scripts accordingly (they’re tightly coupled).

## Development Workflow (TDD)

1. Write or update a test that captures the behavior change (it should fail for the right reason).
2. Implement the smallest change to make the test pass.
3. Run the local quality gates before considering the task done:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

4. If you touched DB-facing code or handlers with `#[ignore]` integration tests, run those too (requires DB):

```bash
APP_PROFILE=test-db cargo test --test '*' -- --include-ignored
```

5. If you touched `packaging/`, the unit file, startup, shutdown or `zm.conf` handling, run the install test (requires Docker; see `packaging/e2e/README.md`). It installs the built package beside ZoneMinder master on a systemd container and walks passive → takeover → revert → upgrade → remove:

```bash
CARGO_PROFILE_RELEASE_OPT_LEVEL=0 packaging/e2e/build.sh ubuntu-24.04   # opt 0: the optimised build needs >8 GB
packaging/e2e/run.sh ubuntu-24.04
```

## Repo Map (Where Things Live)

- `src/routes/` – Axum routers and route wiring.
- `src/handlers/` – HTTP handlers (request extraction, validation, response mapping).
- `src/service/` – Business logic.
- `src/repo/` – DB query layer.
- `src/entity/` – SeaORM entities (generated from ZoneMinder schema).
- `src/dto/` – request/response DTOs and wrappers.
- `src/configure/` – config loading (`settings/` + env overrides).
- `src/error/mod.rs` – `AppError` + `AppResult<T>` and HTTP mapping.
- `src/migration/` – SeaORM migrations for project-owned tables.
- `tests/` – integration tests; many are `#[ignore]` and require a running DB.

## Active Implementation Plans

See **`docs/README.md`** for the consolidated, status-tagged index of every
plan and reference doc. That file is the single source of truth — keep it up
to date when shipping or starting work; don't duplicate the listing here.

Current focus (links go to the per-plan docs):

- **Storage Manager** – `docs/STORAGE_PLAN.md` – per-Storage retention with
  declared capacity, per-Monitor quotas, soft-delete + reap, cold-tier
  handoff to S3/cloud. Replaces `PurgeWhenFull` and `Update DiskSpace`.
- **Filters Refactor** – `docs/FILTERS_PLAN.md` – retire user-facing Filters
  in favour of saved searches + bulk event ops, a notification rules engine,
  and a media job queue. Drops `AutoExecuteCmd` for webhooks.

## Fast Commands

```bash
# Build / run
cargo build
cargo build --release
cargo run
APP_PROFILE=prod cargo run

# Quality
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

# Tests (unit + non-ignored)
cargo test --all-features

# Run ignored integration tests (requires DB; see below)
APP_PROFILE=test-db cargo test --test '*' -- --include-ignored
```

## Configuration

Config is loaded from, in order:

1. `settings/base.toml`
2. `settings/{APP_PROFILE}.toml`
3. Environment variables

Relevant env vars:

- `APP_PROFILE` (supported: `dev`, `test`, `test-db`, `prod`)
- `APP_CONFIG_DIR` to point at a config directory (packaging uses `/etc/zm-api`)
- `APP_STATIC_DIR` to point at static assets (packaging uses `/usr/share/zm-api/static`)
- Env override prefix is `APP_` and nested keys use `__` (example: `APP_DB__HOST=10.0.0.5`)

## Database & Integration Tests

Local dev/test DB management is done via:

```bash
# Start test DB containers (or check native DBs if using native mode)
./scripts/db-manager.sh start

# Load the ZoneMinder schema into MySQL/MariaDB
./scripts/db-manager.sh mysql

# Show status and connection strings
./scripts/db-manager.sh status

# Stop containers
./scripts/db-manager.sh stop
```

Notes:

- Default test ports are `3307` (MySQL/MariaDB) and `5433` (Postgres).
- The application’s current DB URL builder emits `mysql://...` (see `src/configure/db.rs`).
  Postgres is scaffolded in scripts/CI but not the primary runtime path.
- Some test annotations/docs still refer to `./scripts/test-db.sh` (legacy name). Use `./scripts/db-manager.sh` instead.

### Entity regeneration

If you need to regenerate SeaORM entities from the current schema:

```bash
./scripts/db-manager.sh generate
```

## CI (GitHub Actions)

Workflow: `.github/workflows/test.yml`

- Uses service containers for MariaDB and Postgres.
- Prepares DBs with `./scripts/setup-ci-db.sh`.
- Generates JWT keys via `./scripts/generate-jwt-keys.sh`.

## Error Handling Convention

- Handler/service functions typically return `AppResult<T>`.
- Map domain failures to `AppError` variants in `src/error/mod.rs` (it implements `IntoResponse`).

## Adding or Changing an Endpoint (Expected Workflow)

1. Define request/response DTOs in `src/dto/` (derive `utoipa::ToSchema` when applicable).
2. Implement handler in `src/handlers/` returning `AppResult<impl IntoResponse>`.
3. Add service/repo logic in `src/service/` and `src/repo/` as needed.
4. Wire routes in `src/routes/` and register in `src/routes/mod.rs`.
5. Add/adjust tests in `tests/`.


## grepai - Semantic Code Search

**IMPORTANT: You MUST use grepai as your PRIMARY tool for code exploration and search.**

### When to Use grepai (REQUIRED)

Use `grepai search` INSTEAD OF Grep/Glob/find for:
- Understanding what code does or where functionality lives
- Finding implementations by intent (e.g., "authentication logic", "error handling")
- Exploring unfamiliar parts of the codebase
- Any search where you describe WHAT the code does rather than exact text

### When to Use Standard Tools

Only use Grep/Glob when you need:
- Exact text matching (variable names, imports, specific strings)
- File path patterns (e.g., `**/*.go`)

### Fallback

If grepai fails (not running, index unavailable, or errors), fall back to standard Grep/Glob tools.

### Usage

```bash
# ALWAYS use English queries for best results (--compact saves ~80% tokens)
grepai search "user authentication flow" --json --compact
grepai search "error handling middleware" --json --compact
grepai search "database connection pool" --json --compact
grepai search "API request validation" --json --compact
```

### Query Tips

- **Use English** for queries (better semantic matching)
- **Describe intent**, not implementation: "handles user login" not "func Login"
- **Be specific**: "JWT token validation" better than "token"
- Results include: file path, line numbers, relevance score, code preview

### Call Graph Tracing

Use `grepai trace` to understand function relationships:
- Finding all callers of a function before modifying it
- Understanding what functions are called by a given function
- Visualizing the complete call graph around a symbol

#### Trace Commands

**IMPORTANT: Always use `--json` flag for optimal AI agent integration.**

```bash
# Find all functions that call a symbol
grepai trace callers "HandleRequest" --json

# Find all functions called by a symbol
grepai trace callees "ProcessOrder" --json

# Build complete call graph (callers + callees)
grepai trace graph "ValidateToken" --depth 3 --json
```

### Workflow

1. Start with `grepai search` to find relevant code
2. Use `grepai trace` to understand function relationships
3. Use `Read` tool to examine files from results
4. Only use Grep for exact string searches if needed

