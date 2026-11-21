# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Agyva is at `0.1.x`. Anything in `Unreleased` is on `main` but has not been
tagged; release artifacts are built by pushing a `v*` tag (see
[`.github/workflows/release.yml`](.github/workflows/release.yml)).

## [Unreleased]

## [0.1.3] - 2026-11-21

Cross-platform expansion, custom LLM endpoints with local protocol bridge, project profiles, quota burn-rate analytics, and a 5-tab TUI.

### Added

- **Custom endpoints and local bridge**:
  - `agyva endpoint` (`ep`) `presets|add|list|show|use|test|models|update|rename|remove` manages custom LLM providers with API keys sealed in `<config>/endpoints/<id>.ep.snap` (AES-256-GCM, AAD `endpoint:<id>`) and public metadata in `<config>/endpoints.json`.
  - 17 built-in provider presets (`openai`, `anthropic`, `gemini`, `openrouter`, `groq`, `deepseek`, `mistral`, `together`, `fireworks`, `xai`, `perplexity`, `ollama`, `lmstudio`, `vllm`, `litellm`, `azure`, `cohere`), `auto` protocol detection, and wire protocols `openai`, `openai-responses`, `anthropic`, `gemini`, `azure-openai`, `ollama`, `cohere`, and `custom`.
  - Live model discovery (`<cache>/endpoint-models.json`), HTTP rate-limit and OpenRouter credit probing, secret-redacted probe errors, and HTTPS-only (or loopback/local HTTP) URL enforcement.
  - `agyva use ep:<name>` / `agyva use --endpoint <selector>` activates an endpoint and syncs `modelProvider` and `customModelsConfig` in `agy`'s `settings.json` without writing API keys.
  - `agyva bridge` starts a synchronous loopback HTTP/1.1 protocol-translating proxy (`/v1/chat/completions`, `/v1/messages`, `/v1beta/models/*`, model-listing routes).
  - `agyva env` prints shell-quoted exports (`bash`, `zsh`, `fish`, `powershell`, `dotenv`); `agyva exec` launches child commands with resolved endpoint/profile environment variables and an optional ephemeral `--bridge`.
- **Profiles, burn-rate analytics, and statusline**:
  - `agyva profile` (`prof`) `init|create|list|show|which|set|delete|use` manages project-local `.agyva.toml` files (discovered up to `.git`) and global `<config>/profiles.toml`, rejecting secret-looking `env` keys.
  - Bounded quota history (`<cache>/quota-history.json`) computes per-window burn rate (`%/h`) and exhaustion ETA across `usage`, `monitor`, `statusline`, and the TUI.
  - `agyva statusline` renders fast cache-only status summaries in `compact`, `full`, `plain`, `starship`, `tmux`, `waybar`, and `json` formats.
  - `agyva watch` and `agyva monitor` (`mon`) add quota warning/critical thresholds (`--warn-below`, `--critical-below`), `--once`, `--json-stream`, and `--profile`.
  - New `config.toml` keys: `warn_percent` (`25.0`), `critical_percent` (`10.0`), `default_profile`, and `statusline_format` (`compact` | `full` | `json`).
- **5-tab TUI and CLI quality-of-life**:
  - Dashboard expands to 5 tabs (`Accounts`, `Usage`, `Endpoints`, `Routes`, `Diagnostics` via `Tab` / `F1`–`F5`), adding a responsive summary metric strip, rounded borders, smooth block gauges, Accounts sort cycling (`s`), endpoint probe/fetch/activate (`t` / `f` / `Enter`), and route/profile activation (`Enter`).
  - `agyva checkpoints` (`ckpt`) `list|restore|prune` lists, restores (`--force`, `-y`), and prunes encrypted checkpoints (including pinned `*.keep.ckpt`).
  - `agyva route init|add|remove`, `agyva config reset <key>`, `agyva doctor --fix` (safe 0700/0600 permissions, missing directories, stale lockfile cleanup), command aliases (`ls`, `switch`, `ep`, `prof`, `mon`, `st`, `ckpt`), and global `-q` / `--quiet`.
  - `agyva completions` adds `powershell` and `elvish`, and expands Fish dynamic completions to endpoints (`ep:<name>`), profiles, and checkpoints.

### Changed

- **Cross-platform support and raw binary releases**:
  - Keyring integration supports Linux Secret Service, macOS Apple Keychain, and Windows Credential Manager; `updater`, `fsutil`, `discovery` (`PATHEXT` / `.exe`), and `sessions` (`agy.exe`) support cross-platform targets (`x86_64`/`aarch64` Linux `gnu`, macOS, Windows MSVC).
  - CI (`.github/workflows/ci.yml`) adds `macos-latest` and `windows-latest` build/test jobs; release (`.github/workflows/release.yml`) builds 6 targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`) as standalone raw binaries with `SHA256SUMS`; `install.sh` and `agyva update` download and verify raw binaries directly instead of `.tar.gz` archives.
  - Verification note: Linux targets and sandboxed suites are verified locally; macOS Apple Keychain and Windows Credential Manager backends are implemented and CI-configured, not locally tested on macOS or Windows hardware.

## [0.1.2] - 2026-11-21

Auto-switch deadlocks and safety fixes, updater and session hardening, `agyva rekey`, and release installer.

### Added

- `agyva rekey` rotates the local vault key and re-wraps saved snapshots and checkpoints without re-adding accounts.
- `install.sh` downloads, verifies (`SHA256SUMS`), and installs release archives; `scripts/release.sh` accepts `--target <triple>`.
- Configurable auto-switch thresholds (`five_hour_threshold_percent`, `weekly_threshold_percent`), hysteresis (`hysteresis_percent`), per-group `cooldown_secs`, global `auto_switch_cooldown_secs`, and `auto_switch_candidate_max_age_secs`.
- `#![forbid(unsafe_code)]` crate-wide in `src/lib.rs`.

### Fixed

- Auto-switch now triggers when active quota hits threshold with sufficient candidate margin (hysteresis), decouples standby candidate freshness from live active refresh, persists fail-closed cooldown state, and serializes state updates with atomic file locking.
- `agyva monitor` applies backoff on repeated refresh failures and deduplicates repeated `RouteHistory` auto-switch hold/defer entries.
- Self-updater restricts startup checks and installs to supported Linux targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`) and caches passive update checks in `<cache>/update-check.json` (24 h on success, 1 h on failure).
- Session detection scopes process checks to the current UID and uses `flock`-aware lock detection.

### Changed

- Export archives now derive keys with a 64 MiB Argon2id profile (`t=3`, `p=1`), while `import` remains compatible down to the 19 MiB (`t=2`, `p=1`) floor.
- Release workflow (`.github/workflows/release.yml`) splits `build` (`contents: read`) from `publish` (`contents: write`) and re-verifies checksums before drafting the release.

## [0.1.1] - 2026-11-16

Routing, route automation, transfer and self-update, building on `0.1.0`.

### Added

- `agyva export <FILE> [--account <SELECTOR>]` writes a portable archive of
  saved accounts: passphrase-encrypted (a separate archive passphrase, the
  production Argon2id profile), always a fresh absolute-path file, no vault
  key or quota cache inside.
- `agyva import <FILE>` adds accounts from an archive: every entry is
  validated before anything changes, already-saved identities are skipped
  untouched (including legacy email-only logins matched symmetrically), alias
  conflicts only block new records and an id collision with a different
  identity refuses the whole import. New snapshots go through encrypted
  staging, a durable pending marker, no-clobber hard links and a
  registry-last commit; an interrupted import is completed or rolled back
  automatically on the next command.
- `agyva update [--check]` self-updates from GitHub Releases: strictly newer
  stable SemVer only, the release tarball verified against `SHA256SUMS` and
  GitHub's per-asset digest, exact single-executable extraction (links,
  traversal names and duplicates refused) and an atomic rename into place.
- A passive startup update check on every enabled invocation (config
  `check_updates`, default `true`; `AGYVA_UPDATE_CHECK=0` overrides): one
  2-second request, silent on failure, a one-line stderr notice for human CLI
  runs and a dashboard notice in the TUI — never for JSON output,
  completions or the `update` command.
- Cache-only routing groups in a separate `routing.toml`, with Smart and Priority
  rankings, deterministic explanations, and `route list|inspect|plan --json`.
  Stale, incomplete, reset-expired and suspended accounts cannot be recommended.
- A per-account model catalog: a bounded `agy models` run for the active
  account only (config `models_cache_secs`, default one day), parsed into
  effort-suffixed families and versioned into `cache/models.json`. `route
  inspect|plan --for-model <model>` filters eligibility by observed catalog
  support — a stale or missing catalog never proves support, and the static
  reference list is descriptive only. User-declared quota-group mappings
  (`[groups.X.models]`) apply the requested family's windows, specific pools
  before broad ones; plans stay user-declared, never inferred.
- `agyva models [--refresh] [--json]`: the cached model catalog behind
  `--for-model` — each account's families, observation age and freshness.
  Without `--refresh` it never runs agy; `--refresh` forces one bounded
  `agy models` run for the active account only (refresh lock, attribution
  checked before and after, prior observation kept on failure).
- `agyva monitor [--interval 15s] [--group NAME] [--for-model FAMILY]`: a
  route-aware foreground monitor. Evaluation is cache-only (default every
  15s; `15s`/`30`/`2m`/`1h`, 5s floor) and shows the sticky recommendation,
  every account's share, freshness and exclusion reason, and the next weekly
  reset countdown; the active account refreshes on a single-flight 300s
  cadence, and the same tick keeps the active account's model catalog
  current on the `models_cache_secs` throttle (no spawn while fresh). Never
  switches by itself.
- `ranking = "least-used"` in `routing.toml`: orders eligible accounts by
  declared-plan share used (free < pro < ultra); without declared plans it
  degrades to `smart`'s ordering, stated in the docs — never an imagined
  absolute capacity. `smart` remains the default.
- `agyva route apply GROUP`: activate a group's recommended account with one
  journaled switch, confirmed by default (`--yes` non-interactively; with
  `--json` it is required, so stdout stays one JSON value) and
  revalidated under the lock against pending recovery, a changed group file
  and evidence drift before any mutation. `agyva route history` shows a
  bounded, redacted log of routing decisions (applied/refused/failed with
  reasons; never credentials).
- Opt-in auto-switch (`core::auto`): a `routing.toml` group with
  `auto_switch = true` — and only after `agyva config set auto_switch_ack
  true` — is driven by `agyva monitor --group NAME`'s evaluation cycle (15s
  tick, single-flight 300s active refresh; the pane reports an applied
  switch). It switches only on fresh live
  evidence, never picks a suspended or otherwise ineligible account, defers
  while agy sessions are detected (or detection is uncertain) or a switch
  journal awaits recovery, waits at least 900 seconds between any two
  switches (a manual `agyva use` re-arms the cooldown), revalidates under
  the login lock and switches through the journaled path. Holds and defers
  are recorded in `agyva route history`; cooldown bookkeeping lives in
  `<state dir>/auto-switch.json` (0600, schema 1). Off by default.
- `config auto_switch_ack`: explicit, separately-settable acknowledgment for
  the opt-in automation above.

### Known limitations

- The interactive TUI route tab planned for this cycle is deferred: route
  ranking ships in the watch-style `agyva monitor` terminal output instead.

### Fixed

- `--for-model` no longer excludes an account whose requested family still has
  quota just because an unrelated group is exhausted: catalog support and the
  mapped group's windows are evaluated first, and eligibility plus the
  headline share come from that group. An effort-suffixed id
  (`gemini-4-carbon-high`, any case) now inherits its family's declared
  mapping instead of falling back to the whole-account verdict; a standalone
  id that merely ends in an effort word keeps its own identity. Rows are
  re-ranked on the mapped numbers, so position always agrees with the printed
  share: `smart` by tier, `priority` and `least-used` demoting only ineligible
  accounts and then applying their declared order — a rate-capped `free`
  account is never displaced by an `ultra` one.
- A family the catalog offers but no group maps is excluded with the fix
  spelled out, instead of inheriting whole-account eligibility that says
  nothing about that family.
- Auto-switch activates the account its decision and pane named: the
  `--for-model` filter travels into `route apply`, so revalidation ranks the
  same family rather than the whole account.
- The monitor's `agy models` tick respects the suspension marker, the cached
  `TermsSuspended` health and the agy compatibility gate: a suspended or
  unverified account is never spawned against from a background tick, and the
  skip note names `--allow-untested` as the bypass (a refresh never is).
- The catalog cache is pruned with the quota cache and on `agyva rm`, so dead
  account ids no longer linger in `cache/models.json`.
- A routing refusal — and an auto-switch hold or defer — is no longer masked
  by history-lock contention: recording the line is best-effort, so the user
  sees the real decision instead of "another agyva is running".
- `route history --json` honours `--limit` like the human table, `route apply
  --json` emits exactly one JSON value (warnings go to stderr), refusals are
  stamped with the decision clock, and concurrent `RouteHistory` writers are
  serialized by a state lock instead of losing entries.
- Session detection reports "unknown" (unreadable presence directory or
  process table) instead of "no sessions": auto-switch defers and `doctor`
  warns rather than claiming nothing is running.
- Terminal overlays pin confirmation/cancel controls on short screens and scroll
  wrapped Help/Details content. Usage and Diagnostics scroll to their rendered
  final row, including after repeated page navigation.
- Self-update hardening: the archive layout produced by `scripts/release.sh`
  (plain directory entries) now installs, the whole decompressed stream is
  bounded rather than each member, a malformed GitHub per-asset digest is
  refused instead of silently disabling that check, and both the initial
  request and every redirect hop must stay on GitHub's release hosts.
- Import decides a stored-id collision on the full identity before the legacy
  email match, so a 40-bit id collision with a different identity is refused
  instead of being skipped as "already saved". A present but malformed
  companion installation id refuses the archive, and a valid one is persisted
  normalized.
- `agyva config get checkpoints_keep` and `agyva config set policy_acknowledged`
  work (the arms were missing) and a test now asserts every documented key has
  both a getter and a setter.
- `atomic_create` removes its temporary hard-link name after a successful
  create instead of leaking it, and syncs the directory after the unlink.
- The TUI performs at most one passive update check per process, however often
  the dashboard reloads.
- The `after-credential-written` failpoint is registered again, so the switch
  crash test arms a real failpoint.
- Update and archive errors carry hints about the network or the archive
  passphrase instead of the generic credential-store `agyva doctor` advice.
- Accounts show cache freshness and age instead of an unreported plan. Quota is
  labelled remaining and its minimum-per-enabled-group summary is explained;
  freshness ages while the dashboard stays open.
- Narrow footers retain help and quit actions during filters and errors;
  Diagnostics no longer advertises a switch action. Configured monochrome and
  nonempty `NO_COLOR` preserve selection and text status without colors.
- `agyva add` now accepts the consumer credential shape written by `agy 1.5.9`,
  which has no `id_token`. Identity is taken from a decodable `id_token`
  subject, then its email, then a validated plain `id_token_email`, then a
  SHA-256 fingerprint of the refresh token. A malformed `id_token` falls back
  to the remaining fields instead of refusing the credential. Expiry is parsed
  leniently: an unparseable value is reported as unknown, not as a fatal error.
- An email-only login deduplicates case-insensitively against an account that
  was saved from a subject, so the same person is not stored twice.
- The documented `agy 1.5.9` credential shape and identity precedence were
  corrected in the compatibility notes.
- `agyva monitor` and `agyva watch` end cleanly (exit 0) when their stdout
  reader closes (`| head -3`) instead of looping with nowhere to write; any
  other write failure exits non-zero with an error. Ineligible rows in the
  `route plan` and `monitor` tables show no quota percentage, so a stale
  reading cannot be mistaken for available quota.

### Changed

- Task-first CLI help, explicit missing quota windows and cache sources, and a
  shorter README with linked command, dashboard, Antigravity and JSON guides.
- Documentation captures now replay a sanitized existing registry and quota
  cache through the production CLI/TUI renderer. Group values, missing windows,
  health and timestamps are preserved; synthetic captures remain explicit.


## [0.1.0] - 2026-11-15

First published release, with a Linux x86-64 archive and `SHA256SUMS`.

### Added

- Manual account management: `add`, `list`, `use`, `current`, `rename` and
  `remove`, with selectors by alias, list number, exact email, account id or
  unique id prefix, and `-` for the previous account.
- Encrypted vault for saved logins and switch checkpoints: AES-256-GCM with a
  random nonce and authenticated metadata; the vault key in the OS keyring or
  derived with Argon2id from a passphrase; atomic writes and 0600/0700
  permissions.
- Journaled switching with read-back verification, rollback or roll-forward,
  `recover` (and `--rollback`/`--force`), and pinned `*.keep.ckpt` copies for
  logins agyva has not saved.
- Quota from the only supported source, `agy -p "/usage" --output-format json`,
  with cached, stale and not-yet-checked states; per-account health and an
  explicit Terms-of-Service suspension state that is never retried
  automatically.
- `usage` and `health` (with `--all`, `--json`, `--no-refresh`,
  `--force-refresh`) and a read-only foreground `watch`.
- Interactive terminal UI with quota bars, account details, usage and
  diagnostics; confirmation before a switch; secret-free rendering.
- `status`, `doctor`, `config show|set|path`, `version`, and
  `completions fish|bash|zsh`.
- Stable JSON views with a schema version, distinct exit codes (0, 1, 2, 3),
  error hints, and a policy notice recorded once at first use.
- Sandbox and test infrastructure: temporary directories, synthetic
  `example.invalid` credentials, a fake `agy` script, failpoints, a pty smoke
  test, and a synthetic TUI capture script.
- Documentation for architecture, security, compatibility, development and
  release packaging, plus a normalized release archive with `SHA256SUMS`.
