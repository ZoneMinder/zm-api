# Contributing to zm-api

Thanks for your interest in improving zm-api! This guide covers how to get a change
merged.

## 📜 Contributor License Agreement

zm-api is **dual-licensed** — open source under [AGPL-3.0](LICENSE), and available under a
separate commercial license. For the project to be offered under both, every contribution
must be covered by the [Contributor License Agreement](CLA.md).

**By opening a pull request you agree to the [CLA](CLA.md)** for that and all future
contributions. You keep the copyright in your work — the CLA is a license grant, not an
assignment, that lets ZoneMinder Inc. license the project under both AGPL and commercial
terms. Please read it before contributing.

The **CLA Assistant** workflow checks this automatically: if you haven't signed, a bot
comments on your PR and the `CLA Assistant` status check fails. Reply on the PR with:

> I have read the CLA Document and I hereby sign the CLA

Your signature is then recorded in `signatures/version2/cla.json` and the check passes.
You only sign once — future PRs are recognised automatically.

### Maintainer note — branch protection

The CLA check only *blocks* a merge if it is a **required status check**. After the
workflow has run at least once, configure it under
**Settings → Branches → branch protection rule for `master`**:

- Enable **Require status checks to pass before merging** and add **`CLA Assistant`**
  (alongside the `Tests`, `Rustfmt & Clippy`, and `Coverage` checks).
- The workflow needs **Settings → Actions → General → Workflow permissions** set to
  **Read and write** so it can commit signatures to the `cla-signatures` branch.

Without the required-check setting the CLA status is advisory only.

## 🛠️ Development workflow

zm-api works tests-first. Before opening a PR:

1. **Add or update a test** that captures the behaviour change — it should fail for the
   right reason first.
2. **Implement** the smallest change that makes it pass.
3. **Run the quality gates** — all three must be green:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all-features
   ```

4. If you touched DB-facing code, run the integration suite too (needs the test database):

   ```bash
   ./scripts/db-manager.sh start && ./scripts/db-manager.sh mysql
   APP_PROFILE=test-db cargo test --test '*' -- --include-ignored
   ```

   Tests that act on the whole database (the media job runner, maintenance and retention
   passes) use `common::isolated::IsolatedDb`, a database of their own, when
   `TEST_DATABASE_ADMIN_URL` names a server account that can create databases
   (`mysql://root:<password>@127.0.0.1:3307`). Without it they share the test database and
   take turns. Use it for any new test that sweeps or counts rows it didn't make.

CI enforces the same gates plus a line-coverage floor — keep coverage from regressing by
adding tests alongside new code.

## 🧭 Conventions

- Keep changes small and focused; don't reformat or "clean up" unrelated code.
- Treat `src/entity/` and the SeaORM active enums as **generated artifacts** — regenerate
  them with `./scripts/db-manager.sh generate` rather than hand-editing.
- Map domain failures to `AppError` variants in `src/error/`.
- Don't commit secrets; local config profiles like `settings/dev.toml` are gitignored.
- See [`CLAUDE.md`](CLAUDE.md) for the full repo map, commands, and architecture notes.

## 🐛 Reporting issues

Open a GitHub issue with steps to reproduce, the expected vs actual behaviour, and
relevant logs or config (with secrets redacted).

## ✅ Pull request checklist

- [ ] I agree to the [CLA](CLA.md).
- [ ] Tests added/updated and passing.
- [ ] `cargo fmt`, `cargo clippy -D warnings`, and `cargo test` all pass.
- [ ] The change is focused and documented where it isn't self-evident.
