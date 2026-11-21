# Architecture

[README](../README.md) · [Commands](commands.md) · [Security](security.md) · [Development](development.md)

`agyva` is a library (`src/lib.rs`, `#![forbid(unsafe_code)]`) with a thin binary entrypoint (`src/main.rs`).

## Layers

| Module | Role |
| --- | --- |
| `cli` | `clap` parser (`mod.rs`), dispatch (`commands.rs`, `endpoints_cmd.rs`, `profiles_cmd.rs`, `watch_cmd.rs`, `monitor_cmd.rs`), rendering (`output.rs`, `usage_out.rs`). |
| `core` | Domain: `app`, `add`, `switch`, `recover`, `manage`, `rekey`, `checkpoints`, `status`, `doctor`, `endpoints`, `protocol`, `bridge`, `profile`, `app_integrations`, `quota`, `quota_cache`, `quota_history`, `health`, `provider`, `refresh`, `usage`, `watch`, `monitor`, `route`, `route_config`, `route_apply`, `auto`, `catalog`, `dashboard`, `registry`, `selector`, `view`, `failpoint`. |
| `agy` | `agy` discovery, compatibility gate, credential parsing, companion state, session detection, `models` and `/usage` runners/parsers. |
| `storage` | AES-256-GCM `vault`, `vault_key`, OS `keyring`, `fsutil` atomic IO, `lock` (`fs4`), `journal`. |
| `paths`, `config` | Platform dir resolution (`Env` snapshot) and `config.toml`. |
| `notify`, `updater` | Desktop notifications (`notify-rust`) and GitHub Releases updater (`ureq`). |
| `tui` | Ratatui state machine (`app.rs`, `event.rs`), worker (`backend.rs`), `views/*`, `theme.rs`, loop (`mod.rs`). |
| `testing` | Synthetic credential builders for tests/scripts. |

## Storage layout

Default directories come from the `directories` crate (0700 dirs, 0600 files):

| Path | Contents |
| --- | --- |
| `<config>/registry.json` | Public account registry (`id`, `alias`, `email`, `auth_method`, `active`, `previous`, timestamps). |
| `<config>/vault.json` | Key mode (keyring or Argon2id params) and verifier (never the key). |
| `<config>/accounts/<id>.snap` | AES-256-GCM snapshot of credential + companion state. |
| `<config>/endpoints.json` | Public custom endpoint registry (`active_endpoint`, `endpoints[]`). |
| `<config>/endpoints/<id>.ep.snap` | AES-256-GCM API key and secret headers (AAD `endpoint:<id>`). |
| `<config>/profiles.toml` | Global named profiles (`default`, `profiles`); local overrides in `.agyva.toml`. |
| `<config>/routing.toml` | Routing groups, priorities, declared plans, model mappings, auto-switch opt-in. |
| `<config>/config.toml` | User settings (`config.rs`). |
| `<state>/agyva.lock` | Interprocess state lock (`fs4`). |
| `<state>/auto-switch.json` | Fail-closed auto-switch cooldown state. |
| `<state>/route-history.json` | Bounded decision log (up to 50 entries, no credentials). |
| `<state>/journal/pending.json` | Active switch journal. |
| `<state>/checkpoints/*.ckpt` | Pre-switch checkpoints (`checkpoints_keep`, default 5). |
| `<state>/checkpoints/*.keep.ckpt` | Pinned checkpoints for unsaved logins (excluded from normal prune). |
| `<cache>/quota.json` | Cached quota, health, and timestamps (no tokens/emails/raw output). |
| `<cache>/quota-history.json` | Up to 64 samples per `(account_id, group_label, window)` for burn-rate ETA. |
| `<cache>/models.json` | Per-account cached `agy models` family catalog. |
| `<cache>/endpoint-models.json` | Cached endpoint model lists, latency, rate limits, last error. |
| `<cache>/update-check.json` | Cached startup update-check result. |

Account ids are the first 10 lowercase hex characters of `SHA-256(sub)` (falling back to email or refresh-token hash). Disk ids and checkpoint names are validated (`core::ids`) before path construction.

## Switching (journaled)

1. Acquire lock; fail with exit 3 if a journal is pending; check `agy` version gate.
2. Read live `agy` login and write encrypted checkpoint (`*.keep.ckpt` if unsaved and `--force`).
3. Journal `Prepared`; re-read live login to confirm unchanged.
4. Save outgoing account's live credential back to its `.snap`.
5. Re-read live login immediately before write; abort with `ConcurrentChange` if modified externally.
6. Write target credential and apply companion state (`installation_id`, `settings.json` `gcp` block).
7. Journal `CredentialWritten`; read back and verify; journal `Verified`.
8. Commit `registry.json` (`active`, `previous`); journal `Committed`; clear journal; prune old `*.ckpt`.

Non-crash errors roll back automatically except `ConcurrentChange` (nothing written) and `OutcomeUnknown` (keyring write timeout -> stays `RecoveryRequired`, exit 3). `agyva recover` rolls forward from `Verified` and rolls back otherwise (`--rollback` forces rollback; `--force` preserves an unrelated live login in `*.keep.ckpt`).

## Native credential

`agy` stores its login in the OS keyring (`service=gemini`, `username=antigravity`) or `~/.gemini/antigravity-cli/antigravity-oauth-token` (`backend = file`). `storage::keyring` uses `keyring-core` with `zbus-secret-service-keyring-store` (Linux), `apple-native-keyring-store` (macOS), and `windows-native-keyring-store` (Windows). Linux keyring writes are unverified against real `agy`; macOS/Windows backends are CI-tested only.

## Errors and exit codes

`core::error::ErrorKind` maps to exit codes `0` (OK), `1` (error / unhealthy `doctor`), `2` (usage / bad selector / sandbox misuse), `3` (`RecoveryRequired` / `OutcomeUnknown`), and `99` (`AGYVA_TEST_MODE=1` simulated crash). Errors print `error: ...` and `hint: ...` on stderr.

## Sandbox and failpoints

- **Sandbox (`agy::discovery::sandbox`):** `AGYVA_BACKEND` (`file` | `memory`) requires `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR`, and `AGYVA_AGY_HOME` or fails with exit 2.
- **Failpoints (`core::failpoint`):** `AGYVA_FAILPOINT` requires `AGYVA_TEST_MODE=1`.

## Transfer (export/import)

`core::transfer` writes AES-256-GCM archives (`archive:1`) keyed via 64 MiB Argon2id (`t=3`, `p=1`) from `AGYVA_ARCHIVE_PASSPHRASE` or prompt (`import` accepts down to 19 MiB). `import` validates all entries before reading destination state, deduplicates existing identities, rejects alias/id collisions, stages into `accounts/.import-staging` with `.import-pending.json`, publishes via hard links, and reconciles interrupted imports on the next `App::lock`.

## Self-update

`updater.rs` queries GitHub Releases (2s startup timeout, cached 24h OK / 1h error in `<cache>/update-check.json`) on the 6 supported targets (`x86_64`/`aarch64` Linux `gnu`, macOS, Windows `msvc`). `agyva update` requires a newer stable SemVer, downloads the raw binary `agyva-<version>-<target>[.exe]` and `SHA256SUMS`, verifies `SHA256SUMS` and any GitHub API asset digest, and atomically replaces the executable (using `.old` rename-aside on Windows).

## Custom endpoints and local bridge

`core::protocol` implements `EndpointProtocol`, `AuthScheme`, 17 presets, URL validation, model parsing, and rate-limit/credit extraction over `HttpTransport` (`UreqTransport`). `core::endpoints` encrypts secrets (`endpoint:<id>`), caches models, exports env vars, and updates `settings.json` (`modelProvider`, `customModelsConfig`). `core::bridge` runs a synchronous HTTP/1.1 loopback proxy translating OpenAI, Anthropic, and Gemini requests to the target upstream protocol.

## Quota, health, and watch

- **Quota boundary (`core::provider`):** Spawns `agy -p "/usage" --output-format json` (`AGYVA_*` stripped, 15s timeout) for the active account only. `core::refresh` skips suspended accounts (unless `--force-refresh`), serves from cache within `quota_min_refresh_secs` (60s), and preserves prior cache on failure.
- **Auto-switch (`core::auto`):** Runs only in `agyva monitor --group NAME` when both `auto_switch = true` and `auto_switch_ack = true` are set; checks fresh live evidence, candidate max age, thresholds/hysteresis, session detection, pending recovery, and cooldown before running the journaled switch.
- **Health (`core::health`):** `Healthy` requires live quota from the current run; `TermsSuspended` requires explicit ToS text; sticky states survive transient errors.
- **Watch (`core::watch`):** Pure loop with injected clock/jitter/probe/sink, exponential backoff up to 30m, clean exit on SIGINT/SIGTERM/broken pipe.

## TUI

- `app.rs` / `event.rs`: Pure `AppState` and `update(state, event) -> Vec<Job>` state machine.
- `backend.rs`: Single sequential background worker thread (`load`, `switch`, `switch_endpoint`, `test_endpoint`, `fetch_endpoint_models`, `apply_route`, `use_profile`).
- `views/*` & `theme.rs`: Pure Ratatui rendering with ANSI named colors, width-driven `column_plan` (`eta`, `cache age`, `weekly`, `status`, `5H`), and `Paragraph::line_count` scroll bounds.
- `mod.rs`: `TerminalGuard` RAII + panic hook + `ctrlc` signal handling; logging disabled in TUI mode.

## Shell completions

`clap_complete` generates scripts for `fish`, `bash`, `zsh`, `powershell`, and `elvish`. Fish adds dynamic read-only completions (`__agyva_aliases`, `__agyva_endpoints`, `__agyva_profiles`, `__agyva_checkpoints`, `__agyva_use_targets`) with file completion disabled.

## Deferred

- Plan detection and non-`agy` quota sources.
- Local hardware verification for macOS Keychain and Windows Credential Manager.
- Protection against rollback of an older valid encrypted snapshot by a local writer.
