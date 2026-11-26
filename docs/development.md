# Development

[README](../README.md) · [Commands](commands.md) · [Architecture](architecture.md) · [Contributing](../CONTRIBUTING.md)

## Build and check

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
python3 -I scripts/check-links.py
cargo deny check            # requires cargo-deny + network for RustSec DB
```

`make ci` runs format, check, clippy, test, release build, and link check. CI (`.github/workflows/ci.yml`) also tests Rust 1.88 (MSRV) and builds/tests on `macos-latest` and `windows-latest`. `scripts/release.sh` builds standalone release binaries + `SHA256SUMS`.

Rust edition 2024, MSRV 1.88, `#![forbid(unsafe_code)]`.

## Safety rules for tests

Tests must never touch the real OS keyring, `$HOME`, `~/.gemini`, or the real `agy` binary. Everything runs in temporary directories with `example.invalid` identities, synthetic tokens, and `tests/fixtures/fake-agy.sh` (or `fake-agy.cmd` on Windows). Only the ignored `tests/real_keyring.rs` touches Secret Service, using a throwaway item name.

## Environment overrides

| Variable | Effect |
| --- | --- |
| `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR`, `AGYVA_CACHE_DIR` | Override `agyva` directories. |
| `AGYVA_AGY_HOME` | Override `agy` data dir (default `~/.gemini/antigravity-cli`). |
| `AGYVA_AGY_BIN` | Override `agy` binary (tests point to `tests/fixtures/fake-agy.sh` or `fake-agy.cmd`). |
| `AGYVA_BACKEND` | `file` or `memory` sandbox store (never touches OS keyring). **Requires** `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR`, and `AGYVA_AGY_HOME` or `agyva` exits 2. |
| `AGYVA_VAULT_KEY` | `auto`, `keyring`, or `passphrase`. |
| `AGYVA_PASSPHRASE`, `AGYVA_NEW_PASSPHRASE` | Vault passphrase / rekey new passphrase (min 8 chars). Moved into zeroizing holders at startup. |
| `AGYVA_ARCHIVE_PASSPHRASE` | Archive passphrase for `export`/`import` (min 8 chars). |
| `AGYVA_LOG` | `tracing` filter on stderr (disabled in TUI mode). |
| `AGYVA_TEST_MODE` | Set `1` to enable test-only hooks (`AGYVA_FAILPOINT`, interval overrides). |
| `AGYVA_FAILPOINT` | `<name>[:crash\|:error]` (honored only with `AGYVA_TEST_MODE=1`). |
| `AGYVA_WATCH_MAX_CYCLES` | Stop `watch`/`monitor` after N cycles. |
| `AGYVA_WATCH_INTERVAL_MS` | Override `watch` interval in ms (requires `AGYVA_TEST_MODE=1`). |
| `AGYVA_MONITOR_INTERVAL_MS` | Override `monitor` eval interval in ms (requires `AGYVA_TEST_MODE=1`). |
| `AGYVA_UPDATE_CHECK` | `0` disables passive startup update check. |
| `AGYVA_REAL_KEYRING_TEST` | `1` enables ignored `tests/real_keyring.rs`. |

### Failpoints

`AGYVA_FAILPOINT=<name>[:crash|:error]` faults switch/recovery/import steps (`crash` exits 99; `error` triggers normal error/rollback): `after-checkpoint`, `after-prepared`, `after-native-write`, `after-companion`, `after-credential-written`, `verify`, `after-verified`, `after-registry`, `post-commit-cleanup`, `endpoint-clear`, `rollback-after-native`, `recover-after-native`, `import-staged`, `import-marker`, `import-published`, `import-registry` (`src/core/failpoint.rs`).

## Testing strategy

| File | Coverage |
| --- | --- |
| `tests/accounts.rs` | `add`, `list`, `rename`, `remove`, selectors. |
| `tests/transactions.rs` | Switch/recovery failpoints, rollback, roll-forward, concurrent change, keyring timeout, `--force`, pinned checkpoints. |
| `tests/checkpoints.rs` | Checkpoint listing, metadata, restore (`--force`), prune (`--include-pinned`), and malformed filename safety. |
| `tests/security.rs` | Zero-secret checks in files, stdout/stderr, JSON, errors; tampered/wrong-slot envelopes; file modes. |
| `tests/hardening.rs` | Sandbox gating, failpoint gating, id validation, KDF bounds, passphrase length. |
| `tests/endpoints.rs` | Endpoint registry, encrypted secrets, 17 presets, URL validation, model parsing, rate limits, probes. |
| `tests/bridge.rs` | Loopback HTTP/1.1 bridge translation across OpenAI, Anthropic, Gemini, Ollama, Azure, and Cohere. |
| `tests/profiles.rs` | Local `.agyva.toml` discovery (`.git` stop, depth cap), global `profiles.toml`, secret-env refusal. |
| `tests/quota_history.rs` | Bounded quota history sampling, reset-segment isolation, burn-rate and ETA calculation. |
| `tests/usage.rs`, `tests/e2e_usage.rs`, `tests/watch.rs` | Quota parsing, refresh throttles, health states, CLI `usage`/`health`/`watch` with fake `agy`. |
| `tests/tui.rs`, `tests/pty_smoke.rs` | TUI state machine, Ratatui `TestBackend` rendering, PTY smoke test (`tmux`). |
| `tests/e2e.rs`, `tests/cli_v013.rs` | Sandboxed CLI E2E: core switch/recover/exit codes (`e2e.rs`) and v0.1.3 endpoints/bridge/env/exec/profiles/statusline/checkpoints/route/doctor-fix/completions (`cli_v013.rs`). |
| `tests/transfer.rs` | Export/import roundtrip, deduplication, conflicts, import failpoints/recovery. |
| `tests/updater.rs` | SemVer gating, `SHA256SUMS` and digest verification, 6-target coverage, `.exe` naming, atomic replace. |
| `tests/routing.rs`, `tests/route_apply.rs`, `tests/catalog.rs`, `tests/monitor.rs`, `tests/auto_switch.rs` | Route ranking, `--for-model` catalog filtering, `route apply`/history, `monitor` loop, opt-in auto-switch gates. |
| `tests/real_keyring.rs` | Ignored opt-in Secret Service round-trip on throwaway item. |
| `tests/common/mod.rs` | Shared test rig (`MemoryKeyring`, `MemoryBackend`, `FlakyNative`). |
| `src/` unit tests | Parsers, config validation, classification, error hints. |

### Fake `agy` fixtures

- `tests/fixtures/fake-agy.sh` (Unix) and `tests/fixtures/fake-agy.cmd` (Windows): answer `--version` (`agy 1.5.9`), `-p /usage`, and `models` from adjacent `usage.*` / `models.*` files without touching real credentials.

## Scripts

| Script | Purpose |
| --- | --- |
| `scripts/demo-sandbox.sh <dir>` | Create a scratch sandbox with synthetic accounts and print `export` lines. |
| `scripts/capture-tui.sh [--snapshot\|--synthetic] [file]` | Render sanitized cache (default) or synthetic data to text and SVGs via `TestBackend` (requires Python 3). |
| `scripts/check-links.py` | Verify local Markdown links and heading anchors offline. |
| `install.sh` | Install/uninstall raw release binary on Linux or macOS after `SHA256SUMS` check. |
| `scripts/release.sh [--target <triple>]` | Build `--release --locked`, copy `dist/agyva-<version>-<target>[.exe]`, write/verify `dist/SHA256SUMS`. |

```sh
eval "$(scripts/demo-sandbox.sh /tmp/agyva-demo)"
agyva list
```

## Dependencies and conventions

- `deny.toml` enforces licenses, bans, and advisories. Four unmaintained-only notices are time-boxed ignores (`RUSTSEC-2026-0370/0371/0372` for `zbus_names`/`zvariant`/`zvariant_derive`, `RUSTSEC-2026-0543` for `chrono`).
- Use `Env` for environment reads, return `Error` with `ErrorKind` + `hint`, and forbid `unwrap`/`expect`/`panic!`/`unreachable!` in non-test `src/`.
