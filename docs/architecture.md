# Architecture

[README](../README.md) · [Commands](commands.md) · [Security](security.md) · [Development](development.md)

Agyva is one Rust package: a library (`src/lib.rs`) and a thin binary
(`src/main.rs`). Every behavior, including crash recovery, is reachable from
tests through the library.

## Table of Contents

- [Layers](#layers)
- [Storage layout](#storage-layout)
- [Switching (journaled)](#switching-journaled)
- [Native credential](#native-credential)
- [Errors and exit codes](#errors-and-exit-codes)
- [Development and test environment](#development-and-test-environment)
- [Transfer (export/import)](#transfer-exportimport)
- [Self-update](#self-update)
- [Custom endpoints and local bridge](#custom-endpoints-and-local-bridge)
- [Quota provider boundary](#quota-provider-boundary)
- [Health](#health)
- [Watch](#watch)
- [TUI](#tui)
- [Shell completions](#shell-completions)
- [Not yet built (deferred)](#not-yet-built-deferred)

## Layers

| Module | Role |
| --- | --- |
| `cli` | clap definition, dispatch (`commands`, `endpoints_cmd`, `profiles_cmd`, `watch_cmd`, `monitor_cmd`), table/statusline rendering (`output`, `usage_out`). No business logic. |
| `core` | Domain: `App` context, `add`/`switch`/`recover`/`manage`/`rekey`/`checkpoints`/`status`/`doctor`, `endpoints`/`protocol`/`bridge`/`profile`/`app_integrations`, `quota`/`quota_cache`/`quota_history`/`health`/`provider`/`refresh`/`usage`/`watch`/`monitor`/`route`/`route_config`/`route_apply`/`auto`/`catalog`/`dashboard`, `registry`/`selector`/`view`/`failpoint`. |
| `agy` | Everything about agy: credential parsing, native backends, companion state, session detection, version compatibility, model catalog & `/usage` runner/parser. |
| `storage` | Encrypted vault, vault-key provider, OS keyring abstraction, atomic file IO, state lock, journal. |
| `paths`, `config` | Directory resolution (`Env` snapshot, no ad hoc env reads) and `config.toml`. |
| `notify`, `updater` | Best-effort desktop notifications and GitHub Releases self-updater. |
| `tui` | Ratatui state machine (`app.rs`, `event.rs`), background worker (`backend.rs`), views (`views/*`), theme (`theme.rs`). |
| `testing` | Synthetic credential builders (hidden from docs). |

## Storage layout

| Location | Contents |
| --- | --- |
| `<config>/registry.json` | Public account list: id, alias, email, auth method, timestamps, `active`, `previous`; carries `schema_version`. |
| `<config>/vault.json` | Vault key mode (keyring or Argon2id parameters) and a verifier. No key. |
| `<config>/accounts/<id>.snap` | Encrypted credential and companion state for one account. |
| `<config>/endpoints.json` | Public custom endpoint registry (`schema_version`, `active_endpoint`, `endpoints[]`). |
| `<config>/endpoints/<id>.ep.snap` | Encrypted API key and secret headers for one endpoint (AES-256-GCM, AAD `endpoint:<id>`). |
| `<config>/profiles.toml` | Global named profiles (`schema_version`, `default`, `profiles`). Project-local overrides live in `.agyva.toml`. |
| `<config>/routing.toml` | Routing groups, priorities, declared plans, model-group mappings, and auto-switch opt-in. |
| `<config>/config.toml` | Settings and the policy-acknowledged flag. |
| `<state>/agyva.lock` | Interprocess lock (fs4). |
| `<state>/journal/pending.json` | Transaction journal while a switch is in flight. |
| `<state>/checkpoints/*.ckpt` | Encrypted pre-switch checkpoints, newest 5 kept (`checkpoints_keep`). |
| `<state>/checkpoints/*.keep.ckpt` | **Pinned** checkpoints: a login agyva holds no other copy of. Never counted or pruned unless `--include-pinned`. |
| `<cache>/quota.json` | Quota cache: per account id, quota groups, health, timestamps. 0600, written atomically under a lock. No email, no tokens, no raw agy output. |
| `<cache>/quota-history.json` | Bounded quota sample history (up to 64 samples per `(account_id, group_label, window)`) for burn-rate and ETA analytics. |
| `<cache>/models.json` | Per-account cached `agy models` family catalog. |
| `<cache>/endpoint-models.json` | Cached discovered models, latency, rate limits, and last probe error per custom endpoint. |
| `<cache>/update-check.json` | Cached passive GitHub Releases update-check decision (0600). |

Defaults come from the `directories` crate. Files are 0600, directories 0700.

The account id is the first 10 hex characters of SHA-256 of the JWT `sub` in
agy's stored login. The JWT is decoded **without** signature verification and
used only as a label for local files. Ids are validated (`core::ids`: exactly
10 lowercase hex characters) wherever they are read back from disk (registry,
journal) and before any path is built from one; a checkpoint name from a
journal is validated too, so a tampered file cannot steer a path.

## Switching (journaled)

1. Take the lock; refuse if a journal is pending (exit 3); check agy version.
2. Read agy's live login. Write an encrypted checkpoint of it and of the
   companion state. If the live login is one agyva has not saved (only
   reachable with `--force`), the checkpoint is **pinned** (`*.keep.ckpt`) and
   the command says where it is.
3. Journal `Prepared`. Re-read agy's login; if it differs from step 2, stop.
4. Save the outgoing account's current (possibly refreshed) credential back to
   its snapshot.
5. Re-read agy's login **immediately before the write**. If it differs, another
   process changed it: abort with `ConcurrentChange`, nothing written, nothing
   to roll back. (A change after this last read is an inherent, unclosable
   window.)
6. Write the target credential to agy's native store; apply companion state
   (installation id, `gcp` block of `settings.json`).
7. Journal `CredentialWritten`; read back and verify; journal `Verified`.
8. Commit the registry (`active`, `previous`); journal `Committed`; clear;
   prune ordinary checkpoints. A failure *after* the commit (clearing the
   journal, pruning) is a warning: the switch is done.

A failure that is not a crash rolls back from the checkpoint automatically,
except:

- `ConcurrentChange` (nothing was written), and
- `OutcomeUnknown` (a keyring write or delete that timed out; the item may or
  may not have changed). The journal stays `RecoveryRequired`, the command
  exits 3 and the hint is to resolve any keyring unlock prompt and run
  `agyva recover`.

A pinned checkpoint of a switch that was rolled back successfully is dropped
(the login is back in place); it survives a crash or a failed rollback.

After a crash, `agyva recover` rolls forward when the phase is `Verified`
(native store verified, registry commit pending) and rolls back otherwise;
`agyva recover --rollback` always rolls back. Both are idempotent.
`RecoveryRequired` marks a rollback that itself failed.

**Rollback reads native first.** If agy's current login matches neither the
checkpointed one nor the target snapshot, something else changed it since the
interrupted switch and restoring would overwrite it: `recover` refuses unless
`recover --force`, which first writes a pinned checkpoint of the current login.

`status` and `doctor` detect an incomplete journal and exit 3; `doctor` is
read-only (`doctor --fix` applies safe permission/directory/stale-lock repairs).
Use `agyva checkpoints list|restore|prune` to inspect, restore (`--force` keeps
a pinned backup if the live login differs), or prune checkpoints.

## Native credential

agy stores its login in the OS keyring (`service=gemini`,
`username=antigravity`) or, optionally, the file
`~/.gemini/antigravity-cli/antigravity-oauth-token`. `storage::keyring` uses
`keyring-core` with Secret Service on Linux/BSD, Apple Keychain on macOS, and
Windows Credential Manager on Windows. Linux keyring write compatibility with
real `agy` remains **unverified** (see `docs/security.md`); macOS and Windows
backends are implemented and tested in CI, not verified locally on macOS or
Windows hardware.

## Errors and exit codes

Every error has a kind (`core::error::ErrorKind`) that decides the exit code
and supplies a default `hint:` when the call site gave none
(`Error::user_hint`).

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Error (also `doctor` when it finds a problem) |
| 2 | Usage error: arguments, selector, setting, sandbox misuse |
| 3 | Recovery required: a pending interrupted switch, or `OutcomeUnknown` |
| 99 | Simulated crash (failpoint, test mode only) |

Errors print `error: <message>` and, where there is a next step,
`hint: <next step>` on stderr. `--json` views carry `schema_version` and never
contain secrets.

## Development and test environment

See `docs/development.md` for the full table. Two gates matter architecturally:

- **Sandbox gating** (`agy::discovery::sandbox`): `AGYVA_BACKEND` replaces
  agy's credential store and seals the OS keyring, so it is honored only when
  `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR` and `AGYVA_AGY_HOME` are also set;
  otherwise it is a usage error before anything is read. A stray variable can
  therefore never mix real and fake state.
- **Failpoint gating** (`core::failpoint`): `AGYVA_FAILPOINT` is compiled into
  every build (the binary-level tests need it) but honored only with
  `AGYVA_TEST_MODE=1`; otherwise it is ignored with one warning on stderr.

`AGYVA_PASSPHRASE`, `AGYVA_NEW_PASSPHRASE`, and `AGYVA_ARCHIVE_PASSPHRASE` are
taken out of the `Env` map into zeroizing holders in `App::new`, so they are not
cloned around or handed to child processes. They are not removed from the
process environment (that needs `unsafe` in edition 2024).

Failpoint names: `after-checkpoint`, `after-prepared`, `after-native-write`,
`after-companion`, `after-credential-written`, `verify`, `after-verified`,
`after-registry`, `post-commit-cleanup`, `rollback-after-native`,
`recover-after-native`, `import-staged`, `import-marker`, `import-published`,
`import-registry`. `crash` (default) exits 99 with no cleanup; `error`
fails the step normally and exercises automatic rollback.

## Transfer (export/import)

`core::transfer` implements `agyva export` / `agyva import` over portable,
passphrase-encrypted archives: a versioned envelope holding AES-256-GCM
ciphertext (the dedicated `archive:1` context) whose key comes from a separate
archive passphrase under the 64 MiB Argon2id export profile (`t=3`, `p=1`,
16-byte salt), while `import` accepts archives down to the 19 MiB (`t=2`,
`p=1`) floor for compatibility with existing archives. The payload is only
`{record, snapshot}` entries — no vault key, no `active` pointer, no quota
cache. Export always creates a new file (no clobber) and requires an absolute
path.

Import validates every entry (including ones that will be skipped) before
reading destination state, then plans under the state lock: identities already
saved are skipped unchanged (symmetric legacy match: subjectless-logins with
equal emails count as the same person), alias conflicts apply only to new
records, and an id already saved with a different identity refuses the whole
import. New snapshots are published through encrypted staging files under
`accounts/.import-staging`, a durable marker `config/.import-pending.json`
written before any final snapshot, no-clobber hard links and a registry-last
commit. `App::lock` runs `reconcile_import` on every mutating command, so a
crash at any failpoint is completed or rolled back (by inode ownership) on the
next run; unexpected staging entries are preserved and reported.

## Self-update

`updater.rs` checks GitHub Releases (one GET, 2 s, silent) on supported
targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, and
`aarch64-pc-windows-msvc`), caching results in `<cache>/update-check.json` (24
hours on success, 1 hour on failure) and skipping checks for `--json` and
`completions` invocations. `agyva update` installs: stable SemVer must be
strictly newer, the `agyva-<version>-<target>.tar.gz` asset plus `SHA256SUMS`
are downloaded and verified (file sums and the GitHub per-asset digest when
exposed), exactly `<stem>/agyva` (`<stem>/agyva.exe` on Windows) is extracted
(single regular executable; links, traversal names and duplicates refused) and
the binary is staged next to the running one and replaced atomically (using a
`.old` rename-aside on Windows). The `Transport` trait is the seam: production
uses `ureq` with `https_only`, tests inject in-memory transports. The notice
reaches humans on stderr and the TUI through `Dashboard::update_notice`, never
stderr under the alternate screen.

## Custom endpoints and local bridge

`core::protocol` defines `EndpointProtocol` (`auto`, `openai`,
`openai-responses`, `anthropic`, `gemini`, `azure-openai`, `ollama`, `cohere`,
`custom`), `AuthScheme`, 17 built-in `ProviderPreset`s, URL validation
(`https://` or loopback/local `http://`), model-list parsing, and rate-limit /
OpenRouter credit extraction over the `HttpTransport` seam (`UreqTransport` in
production; mocks in tests). `core::endpoints` seals API keys and secret headers
in `<config>/endpoints/<id>.ep.snap` (AAD `endpoint:<id>`), stores public
metadata in `endpoints.json`, caches discovered models in
`<cache>/endpoint-models.json`, exports shell env vars, and syncs
`modelProvider` / `customModelsConfig` in `agy`'s `settings.json` without
writing secrets. `core::bridge` runs a pure-std synchronous HTTP/1.1 loopback
proxy (`agyva bridge`, `agyva exec --bridge`) translating OpenAI, Anthropic, and
Gemini inbound requests to any configured upstream protocol.

## Quota provider boundary

Quota has exactly one source: `agy -p "/usage" --output-format json`, run by
`agy::usage_run` (no stdin, fixed arguments, no `AGYVA_*` variables in the
child's environment, bounded output, 15 s timeout, killed on timeout).
`agy::usage_parse` accepts only the shapes it recognises; anything else is
`Unavailable(schema_changed)`, never a guess. `core::provider::QuotaProvider`
is the seam: the real provider shells out to agy, tests inject fakes, and the
TUI never builds a quota value itself (a test scans `src/tui` for this).

agy answers for **the currently logged-in account only**. Agyva therefore
refreshes only the active account; it never copies a credential or calls a
Google endpoint to learn about another one. The single exception to
"never switches" is opt-in: `core::auto` may run the same journaled switch
for a `routing.toml` group that sets `auto_switch = true` **and** whose
acknowledgement is in `config.toml`, gated by a configurable cooldown
(`auto_switch_cooldown_secs` / per-group `cooldown_secs`, default 900s) over
fail-closed `auto-switch.json` state, a fresh live reading, active thresholds
and hysteresis, standby candidate eligibility up to
`auto_switch_candidate_max_age_secs`, the session gate (`flock` liveness probe on
`presence/*.lock` plus current-UID process filtering; detected or uncertain
defers), recovery state, and lock-held revalidation immediately before switching.
Default is off. Other accounts show their last cached values, labelled with
their age (`cache`, `stale`), or "not checked yet". `plan` is shown as
unavailable because `/usage` does not report it.

`core::refresh` rules, in order:
1. A pending interrupted switch blocks refreshing (exit 3 in the CLI).
2. A `TermsSuspended` account is never refreshed unless `--force-refresh`.
3. Refreshes closer than `quota_min_refresh_secs` (60) are answered from the
   cache without running agy. Data older than `quota_fresh_secs` (300) is
   labelled stale.
4. A failed or throttled refresh never invents or upgrades data; the previous
   cache entry is kept and the failure is recorded.

## Health

`core::health::classify` reads agy's stderr/exit status/non-quota stdout and
returns a state plus a short rule description (never raw output).
`TermsSuspended` needs an explicit Terms of Service violation message; a bare
403 or `PERMISSION_DENIED` is `Unknown`. `Healthy` needs quota data agy
returned in this run; health read back from the cache is never `Healthy`.
Sticky states (suspended, revoked, login required, token expired) survive
transient failures (offline, unknown) until explicit contrary evidence.

## Watch

`agyva watch` is a foreground, read-only observer. The loop in `core::watch`
is pure: clock, sleeping, jitter, probe and output are injected. It refreshes
the active account every `watch_interval_secs` (300), backs off
exponentially (jittered ±20%) up to 30 minutes on transient failures, never
switches accounts itself, never warms up quota, skips suspended accounts, and stops cleanly on
SIGINT/SIGTERM (exit 0). Optional desktop notifications (`notify`) are
best-effort. `AGYVA_WATCH_MAX_CYCLES` bounds the loop for tests.

## TUI

Bare `agyva` opens the TUI when stdin and stdout are TTYs; otherwise it prints
help. Layers in `src/tui`:

| File | Role |
| --- | --- |
| `app.rs` | `AppState` (plain data): view, data, selections by account/endpoint/route id, sort order, filter, overlay, flags. |
| `event.rs` | Pure state machine `update(&mut AppState, Event) -> Vec<Job>`. Key handling, quit deferral, confirmation. No IO. |
| `backend.rs` | `Backend` trait (`load`, `switch`, `switch_endpoint`, `test_endpoint`, `fetch_endpoint_models`, `apply_route`, `use_profile`), `AppBackend`, and `spawn_worker`. |
| `views/*` | Pure render functions into a ratatui `Frame` (Accounts, Usage, Endpoints, Routes, Diagnostics, chrome, overlays). |
| `theme.rs` | ANSI named colours only (no RGB), so it follows the terminal theme. |
| `mod.rs` | Terminal ownership and the loop. |

**Worker.** All slow work (reading the cache, running agy, switching, probing
endpoints, applying routes/profiles) happens on one background thread that
processes jobs sequentially and reports `Msg`s over a channel. The UI thread
never blocks on agy or HTTP probes. Each job builds a fresh `App`. Startup
queues a cache-only load, then a normal refresh; `r` queues a normal refresh
(never forced) unless one is already in flight. After a switch only a cache read
follows, never an agy run. The thread is not joined at exit.

**Switching** uses the same `core` switch path as `agyva use` (journal,
checkpoint, verification). Enter opens a y/n confirmation that warns when agy
is running. `q`/Ctrl+C during a switch is deferred until it finishes. A vault
in passphrase mode without `AGYVA_PASSPHRASE` cannot prompt inside the TUI:
the switch is refused with a pointer to `agyva use` in a shell.

**Rendering.** Redraw only when the dirty flag is set (events, worker
messages, resize, a 30 s tick for relative times). Column layout is a pure
function of width (`column_plan`: drops `eta`, `cache age`, `weekly`, `status`,
`5H` in that order); text is truncated by unicode display width. Selection
follows stable ids. Freshness is reclassified on the 30 s tick with the
configured policy; historical observations never become a permanent "just now"
label. Scrollable views measure actual wrapped rows with Ratatui’s
`Paragraph::line_count`. Modal controls occupy a reserved final row. Monochrome
rendering removes colors while preserving bold/reverse selection and text
statuses.

**Terminal safety.** A `TerminalGuard` (RAII) enters raw mode and the
alternate screen and restores them on every exit path; a panic hook restores
before the message prints; SIGINT/SIGTERM (via `ctrlc`) set a stop flag that
the loop treats as a quit request. Logging is not initialised in TUI mode so
tracing cannot write over the screen.

## Shell completions

`agyva completions fish|bash|zsh|powershell|elvish` prints a script generated by
`clap_complete`. The Fish script additionally defines `__agyva_aliases`,
`__agyva_endpoints`, `__agyva_profiles`, `__agyva_checkpoints`, and
`__agyva_use_targets` (running read-only `--json` subcommands with no keyring
access) and turns file completion off since no argument is a path. Install to
`~/.config/fish/completions/agyva.fish`.

## Not yet built (deferred)

- Plan detection; any quota source other than agy's own `/usage`.
- Local hardware verification for macOS Keychain and Windows Credential Manager
  backends (implemented and CI-configured, not locally verified).
- Protection against rollback of an old encrypted blob (see `docs/security.md`).
