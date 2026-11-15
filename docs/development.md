# Development

## Build and check

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
cargo deny check            # needs cargo-deny and network access for the RustSec database
```

CI runs the same set (`.github/workflows/ci.yml`). `scripts/release.sh` builds
and packages a release (see the README).

The package is a library (`src/lib.rs`) plus a thin binary; everything,
including crash recovery, is reachable from tests through the library.
Rust 2024, minimum 1.88. `unsafe` is not used.

## Safety rules for tests

Tests and scripts must never touch the real OS keyring, the real `$HOME`,
`~/.gemini`, or the real `agy` executable. Everything runs in temporary
directories with synthetic data (addresses at `example.invalid`, fake tokens)
and a fake `agy` script. The only exception is the ignored real-keyring test,
which is opt-in and restricted to a throwaway service name.

## Environment overrides

These exist for tests and sandboxes. Normal use needs none of them.

| Variable | Effect |
| --- | --- |
| `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR`, `AGYVA_CACHE_DIR` | Override agyva's directories. |
| `AGYVA_AGY_HOME` | `agy` data directory (default `~/.gemini/antigravity-cli`). |
| `AGYVA_AGY_BIN` | `agy` executable used for `--version` and `/usage`. Tests point it at `tests/fixtures/fake-agy.sh`. |
| `AGYVA_BACKEND` | `file` or `memory`: sandbox mode. The OS keyring is never touched and the vault key falls back to a passphrase. **Requires** `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR` and `AGYVA_AGY_HOME` too, otherwise agyva refuses (a stray variable must not mix real and fake state). `memory` keeps the login inside one process; use `file` for multi-command scripts. |
| `AGYVA_VAULT_KEY` | `auto`, `keyring` or `passphrase`. |
| `AGYVA_PASSPHRASE` | Vault passphrase (minimum 8 characters), never read from argv. Moved into a zeroizing holder at startup; it cannot be removed from the process environment without `unsafe`. |
| `AGYVA_LOG` | `tracing` filter; stderr. Disabled in TUI mode. |
| `AGYVA_TEST_MODE` | Set to `1` to enable test-only hooks (currently `AGYVA_FAILPOINT`). |
| `AGYVA_FAILPOINT` | `<name>[:crash\|:error]`. Honored only with `AGYVA_TEST_MODE=1`; otherwise ignored with one warning on stderr. See below. |
| `AGYVA_WATCH_MAX_CYCLES` | Stop `agyva watch` after N cycles. |
| `AGYVA_WATCH_INTERVAL_MS` | Override the watch interval in milliseconds; needs `AGYVA_TEST_MODE=1` (otherwise ignored). |
| `AGYVA_REAL_KEYRING_TEST` | `1` enables the ignored real-keyring test. |

### Failpoints

`AGYVA_FAILPOINT=<name>[:crash|:error]` makes a step of the switch or recovery
fail. `crash` (default) stops the process at once with exit 99 and no cleanup;
`error` fails the step normally and exercises automatic rollback. Names are in
`src/core/failpoint.rs` (`after-checkpoint`, `after-prepared`,
`after-native-write`, `after-companion`, `after-credential-written`, `verify`,
`after-verified`, `after-registry`, `post-commit-cleanup`,
`rollback-after-native`, `recover-after-native`). The hook can only make
agyva fail itself.

## Testing strategy

| File | What it covers |
| --- | --- |
| `tests/accounts.rs` | add / list / rename / remove / selectors. |
| `tests/transactions.rs` | Switch and recovery: every failpoint, rollback, roll-forward, idempotent recovery, concurrent change of agy's login, keyring timeout (unknown outcome), `--force` and pinned checkpoints, post-commit cleanup warning. |
| `tests/security.rs` | No secret in files, output, errors or JSON; wrong-slot and tampered envelopes; permissions. |
| `tests/hardening.rs` | Sandbox gating, failpoint gating, account id validation, KDF bounds, passphrase length. |
| `tests/usage.rs`, `tests/e2e_usage.rs`, `tests/watch.rs` | Quota parsing, refresh rules, health classification, `usage`/`health`/`watch` through the real binary with the fake `agy`. |
| `tests/tui.rs`, `tests/pty_smoke.rs` | TUI state machine and rendering with a test backend; a pty smoke run. |
| `tests/e2e.rs` | The real binary in a sandbox: add, use, crash and recover, exit codes. |
| `tests/real_keyring.rs` | Ignored, opt-in; throwaway service name only. |
| unit tests under `src/` | Parsers, classification, error hints, config validation. |

The harness in `tests/common/mod.rs` builds an `App` over in-memory fakes
(`MemoryKeyring`, `MemoryBackend`, a static passphrase, cheap KDF parameters)
and a `FlakyNative` backend that can change agy's login between reads or time
out a write, so races and unknown outcomes are reproducible without sleeping.

### The fake `agy`

`tests/fixtures/fake-agy.sh` stands in for `agy`. It answers `--version`
(`agy 1.5.9`) and `-p /usage`, taking its behavior from files next to the
script (`usage.out`, `usage.err`, `usage.code`, `usage.sleep`, `usage.calls`).
It never reads or writes any credential.

## Scripts

| Script | Purpose |
| --- | --- |
| `scripts/demo-sandbox.sh <dir>` | Build a synthetic sandbox (fake accounts, fake agy, scratch HOME) and print `export` lines. Use it to try agyva without touching anything real. |
| `scripts/capture-tui.sh [file]` | Capture the TUI from a sandbox into `docs/tui-capture.txt` (needs `tmux`; private socket). |
| `scripts/release.sh` | `cargo build --release --locked`, package into `dist/`, write and check `SHA256SUMS`. |

```sh
eval "$(scripts/demo-sandbox.sh /tmp/agyva-demo)"   # bash/zsh
agyva list
```

## Dependencies and policy checks

`deny.toml` configures `cargo deny` (advisories, licenses, bans, sources).
Three advisories are ignored with a reason each: RUSTSEC-2026-0370/0371/0372
mark `zbus_names`, `zvariant` and `zvariant_derive` as unmaintained (no
vulnerability); they arrive through `zbus`, which the Secret Service store and
desktop notifications need. Revisit when `zbus` moves off them.

## Conventions

- No ad hoc environment reads: the process environment is snapshotted into
  `Env` once.
- Errors carry a kind (which decides the exit code) and an optional hint; add
  a hint when there is a concrete next step.
- Non-test code under `src/` contains no `unwrap`, `expect`, `panic!` or
  `unreachable!`; failures are returned as `Error`.
