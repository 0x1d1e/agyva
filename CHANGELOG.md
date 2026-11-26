# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`agyva` is at `0.1.x`. Release artifacts are built on `v*` tags via [`.github/workflows/release.yml`](.github/workflows/release.yml).

## [Unreleased]

## [0.1.4] - 2026-11-26

### Security

- Endpoint redirects are followed by agyva, not ureq. A host change drops `Authorization`, `x-api-key`, `x-goog-api-key`, `api-key`, custom auth headers, and every other secret header. Scheme downgrade and non-local `http` are refused.
- `agyva bridge --local-token-env` requires `Authorization: Bearer <token>` on every non-OPTIONS request. A missing or wrong token is 401 and is not forwarded. OPTIONS stays unauthenticated and is not forwarded.
- Windows token and vault writes set a user-only DACL. `agyva doctor` reports that ACL (or `not checked`) and does not claim Unix `0600` when it did not look. Directory fsync and file identity fail closed when Windows cannot provide them.
- Windows self-update renames the running binary aside to a pid-and-counter `.old` name and reports both paths if rollback also fails.

### Fixed

- `agyva use` no longer reports failure after the login already changed. A failed custom-endpoint clear is a warning that names what is still set and the retry (`agyva use <alias>`).

### Added

- `agyva auto --model gemini|claude` live-reads the active account and switches through the journaled path when that family's usage is over 90% (remaining under 10%). Other accounts come from cache. Cooldown is 300s and a manual `use` still wins. It does not bypass suspensions. A running agy keeps its loaded login; the command always prints the restart notice.

## [0.1.3] - 2026-11-21

Cross-platform support, custom LLM endpoints with local protocol bridge, project/global profiles, quota burn-rate analytics, and a 5-tab TUI.

### Added

- **Custom endpoints and local bridge:**
  - `agyva endpoint` (`ep`) `presets|add|list|show|use|test|models|update|rename|remove` manages custom LLM endpoints with encrypted API keys in `<config>/endpoints/<id>.ep.snap` (AES-256-GCM, AAD `endpoint:<id>`) and public metadata in `<config>/endpoints.json`.
  - 17 built-in presets (`openai`, `anthropic`, `gemini`, `openrouter`, `groq`, `deepseek`, `mistral`, `together`, `fireworks`, `xai`, `perplexity`, `ollama`, `lmstudio`, `vllm`, `litellm`, `azure`, `cohere`), `auto` protocol detection, and protocols `openai`, `openai-responses`, `anthropic`, `gemini`, `azure-openai`, `ollama`, `cohere`, `custom`.
  - Live model discovery (`<cache>/endpoint-models.json`), rate-limit and OpenRouter credit probing, secret-redacted probe errors, and HTTPS (or loopback/local HTTP) URL validation.
  - `agyva use ep:<name>` and `agyva use --endpoint <selector>` sync `modelProvider` and `customModelsConfig` into `agy`'s `settings.json` without writing API keys.
  - `agyva bridge` starts a synchronous loopback HTTP/1.1 bridge (`/v1/chat/completions`, `/v1/messages`, `/v1beta/models/*`, model-listing routes).
  - `agyva env` prints shell exports (`bash`, `zsh`, `fish`, `powershell`, `dotenv`); `agyva exec` runs commands with resolved endpoint/profile env and optional `--bridge`.
- **Profiles, burn-rate analytics, and statusline:**
  - `agyva profile` (`prof`) `init|create|list|show|which|set|delete|use` manages local `.agyva.toml` (discovered up to `.git`) and global `<config>/profiles.toml`, rejecting secret-like `env` keys.
  - Bounded quota history (`<cache>/quota-history.json`, up to 64 samples per series) computes burn rate (`%/h`) and exhaustion ETA across `usage`, `monitor`, `statusline`, and the TUI.
  - `agyva statusline` renders cache-only summaries in `compact`, `full`, `plain`, `starship`, `tmux`, `waybar`, and `json`.
  - `agyva watch` and `agyva monitor` (`mon`) add `--warn-below`, `--critical-below`, `--once`, `--json-stream`, and `--profile`.
  - New `config.toml` keys: `warn_percent` (`25`), `critical_percent` (`10`), `default_profile`, and `statusline_format` (`compact` | `full` | `json`).
- **5-tab TUI and CLI improvements:**
  - Dashboard expands to 5 tabs (`Accounts`, `Usage`, `Endpoints`, `Routes`, `Diagnostics` via `Tab` / `F1`-`F5`) with summary strip, rounded borders, smooth block gauges, Accounts sort (`s`), endpoint probe/fetch/activate (`t`/`f`/`Enter`), and route/profile activation (`Enter`).
  - `agyva checkpoints` (`ckpt`) `list|restore|prune` manages encrypted checkpoints (`--force`, `-y`, `--include-pinned`).
  - `agyva route init|add|remove`, `agyva config reset <key>`, `agyva doctor --fix`, command aliases (`ls`, `switch`, `ep`, `prof`, `mon`, `st`, `ckpt`), and global `-q` / `--quiet`.
  - `agyva completions` adds `powershell` and `elvish`, and extends Fish dynamic completions to endpoints (`ep:<name>`), profiles, and checkpoints.

### Changed

- **Cross-platform support and raw binary releases:**
  - Keyring layer supports Linux Secret Service, macOS Keychain, and Windows Credential Manager; `updater`, `fsutil`, `discovery` (`PATHEXT` / `.exe`), and `sessions` (`agy.exe`) support Linux, macOS, and Windows.
  - CI (`.github/workflows/ci.yml`) adds `macos-latest` and `windows-latest` jobs; release (`.github/workflows/release.yml`) ships standalone raw binaries + `SHA256SUMS` (no `.tar.gz`) for 6 targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`). `install.sh` and `agyva update` consume raw binaries directly.
  - Evidence boundary: Linux and sandboxed tests are verified locally; macOS Keychain and Windows Credential Manager are implemented and CI-configured, unverified locally on hardware.

## [0.1.2] - 2026-11-21

Auto-switch safety fixes, updater and session hardening, `agyva rekey`, and release installer.

### Added

- `agyva rekey` rotates the vault key or migrates between `keyring` and `passphrase` modes without re-adding accounts.
- `install.sh` installer and `scripts/release.sh --target <triple>`.
- Configurable auto-switch thresholds (`five_hour_threshold_percent`, `weekly_threshold_percent`), `hysteresis_percent`, per-group `cooldown_secs`, `auto_switch_cooldown_secs`, and `auto_switch_candidate_max_age_secs`.
- `#![forbid(unsafe_code)]` in `src/lib.rs`.

### Fixed

- Auto-switch triggers when active quota hits threshold with sufficient candidate margin, decouples standby freshness from live active refresh, persists fail-closed cooldown state, and serializes updates with atomic locking.
- `agyva monitor` backs off on repeated refresh failures and deduplicates repeated `RouteHistory` auto-switch hold/defer entries.
- Self-updater caches passive checks in `<cache>/update-check.json` (24h OK, 1h error) and restricts checks/installs to supported targets.
- Session detection filters process checks to current UID and uses `flock`-aware lock checks.

### Changed

- Export archives derive keys with 64 MiB Argon2id (`t=3`, `p=1`); `import` accepts down to 19 MiB (`t=2`, `p=1`).
- Release workflow splits `build` (`contents: read`) from `publish` (`contents: write`) and re-verifies checksums.

## [0.1.1] - 2026-11-16

Routing, opt-in route automation, export/import transfer, and self-update.

### Added

- `agyva export <FILE> [--account <SELECTOR>]` and `agyva import <FILE>` for portable passphrase-encrypted archives with whole-archive pre-validation, symmetric legacy email deduplication, and crash-safe staging/reconciliation.
- `agyva update [--check]` and passive startup update check (`check_updates = true`, `AGYVA_UPDATE_CHECK=0` opt-out).
- Cache-only `routing.toml` groups with `smart`, `priority`, and `least-used` rankings, `route list|inspect|plan|apply|history`, and `--for-model` catalog filtering.
- `agyva models [--refresh] [--json]` per-account model catalog cache (`<cache>/models.json`).
- `agyva monitor` route-aware foreground loop with single-flight active refresh.
- Opt-in auto-switch (`core::auto`) gated by `auto_switch = true`, `config set auto_switch_ack true`, fresh live evidence, cooldown, and session checks.

### Fixed

- `--for-model` maps effort-suffixed ids and evaluates mapped quota groups independently of unrelated exhausted buckets.
- Catalog cache (`models.json`) prunes with quota cache and on `remove`.
- Best-effort `RouteHistory` lock handling so refusals/holds are not masked; `route history --json` honors `--limit` and `route apply --json` emits one JSON object.
- Session detection reports `unknown` when unreadable so auto-switch defers safely.
- TUI modal scrolling, `NO_COLOR` / `color = never` support, and single startup update check per process.
- Consumer `agy 1.5.9` credential shape support (`id_token` optional, `id_token_email` and `refresh_token` fingerprint fallbacks, lenient expiry parsing).
- Clean exit `0` on broken stdout pipe (`| head`) in `watch` and `monitor`.

### Changed

- Task-first CLI help and sanitized real-cache documentation captures.

## [0.1.0] - 2026-11-15

First release with Linux `x86_64` build and `SHA256SUMS`.

### Added

- Manual account management (`add`, `list`, `use`, `current`, `rename`, `remove`) and selectors.
- AES-256-GCM vault with OS keyring or Argon2id passphrase, atomic 0600/0700 writes, journaled switching, checkpoints, and `recover`.
- Quota/health via `agy -p "/usage" --output-format json`, `usage`, `health`, `watch`, TUI, `status`, `doctor`, `config`, `version`, `completions`, `--json`, and exit codes `0..3`.
- Sandbox harness, fake `agy`, failpoints, and PTY smoke tests.
