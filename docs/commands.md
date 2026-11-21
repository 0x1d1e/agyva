# Commands

[README](../README.md) · [Dashboard](dashboard.md) · [JSON and exit codes](json.md) · [Security](security.md)

Run `agyva --help` for task groups or `agyva <command> --help` for flags. Global flags: `--allow-untested` (bypass `agy` version gate) and `-q` / `--quiet` (suppress notices). Bare `agyva` opens the TUI on a TTY, or prints help otherwise.

## Save and manage accounts

```sh
agyva add --alias personal            # save agy's current login
agyva add --alias personal --refresh  # update saved credential
agyva list                            # alias: ls
agyva current
agyva rename personal home
agyva remove work                     # flags: -y/--yes, --force (if active)
```

Sign in with `agy` before `add`. `remove` deletes only `agyva`'s encrypted snapshot, leaving `agy`'s live login intact.

## Switch and restart

```sh
agyva use personal              # alias: switch
agyva use 2
agyva use -                     # previous OAuth account
agyva use ep:openai             # activate custom endpoint
agyva use --profile work        # activate profile account and/or endpoint
```

Selectors match alias, 1-based list number, exact email, account id, unique id prefix, `ep:<name>`, or `-`. Explicit flags `--account`, `--endpoint`, and `--profile` avoid selector ambiguity (ambiguous selectors exit 2).

If `agy` holds an unsaved login, `use` refuses unless `--force` is passed (which keeps an encrypted pinned backup first). Switching to an OAuth account clears `modelProvider` and `customModelsConfig` in `agy`'s `settings.json`.

**Restart `agy` after switching.** Running sessions keep their startup login. Exit 3 requires `agyva recover`.

## Custom endpoints, bridge, env, and exec

```sh
agyva endpoint presets                # alias: ep
agyva endpoint add openai --preset openai --api-key-env OPENAI_API_KEY --use
agyva endpoint list
agyva endpoint show openai
agyva endpoint test openai
agyva endpoint models openai --refresh
agyva endpoint update openai --model gpt-4.1
agyva endpoint rename openai prod-openai
agyva endpoint use --clear
agyva endpoint remove prod-openai -y
```

API keys are read via `--api-key-env <VAR>`, `--api-key-stdin`, or a hidden prompt (never argv), encrypted in `<config>/endpoints/<id>.ep.snap`, and never written to `endpoints.json`, `settings.json`, or `--json`.

- **17 presets:** `openai`, `anthropic`, `gemini`, `openrouter`, `groq`, `deepseek`, `mistral`, `together`, `fireworks`, `xai`, `perplexity`, `ollama`, `lmstudio`, `vllm`, `litellm`, `azure`, `cohere`.
- **Protocols (`--protocol`):** `auto`, `openai`, `openai-responses`, `anthropic`, `gemini`, `azure-openai`, `ollama`, `cohere`, `custom`.
- **Auth schemes (`--auth-scheme`):** `bearer`, `x-api-key`, `x-goog-api-key`, `api-key`, `none`, `custom-header:<name>[:<prefix>]`.
- **URL policy:** Remote hosts require `https://`; `http://` is allowed only for loopback/local hosts (`localhost`, `*.localhost`, `*.local`, `*.internal`, loopback IPs, RFC1918 IPv4, `fc00::/7` IPv6). Embedded `user:pass@` is refused.
- **`agy` sync:** `endpoint use` and `use ep:<name>` write `modelProvider` and `customModelsConfig` (without secrets) into `agy`'s `settings.json`.

### Bridge, env, and exec

```sh
agyva bridge --endpoint openai --port 8787
agyva env --endpoint openai --shell bash
agyva exec --endpoint openai --bridge -- my-tool --flag
```

- **`agyva bridge`:** Synchronous loopback HTTP/1.1 bridge (default `--listen 127.0.0.1:0`, optional `--port`, `--local-token-env <VAR>`). Accepts OpenAI Chat (`/v1/chat/completions`), Anthropic Messages (`/v1/messages`), Gemini (`/v1beta/models/{model}:generateContent`, `:streamGenerateContent`), and model-listing routes (`/v1/models`, `/v1beta/models`, `/api/tags`).
- **`agyva env`:** Prints shell exports (`--shell bash|zsh|fish|powershell|dotenv`) for `--endpoint`, `--profile` (or active profile/endpoint), `--model`, and `--bridge-url`.
- **`agyva exec`:** Injects resolved profile, account (`AGYVA_ACCOUNT`, `AGYVA_ACCOUNT_ID`), and endpoint env vars into `-- <COMMAND> [ARGS...]`. `--bridge` runs an ephemeral `127.0.0.1:0` bridge for the child lifetime.

## Project and global profiles

Store non-secret defaults (`account`, `endpoint`, `group`, `model`, `warn_percent`, `critical_percent`, `monitor_interval`, `description`, `env`) in project-local `.agyva.toml` or global `<config>/profiles.toml`.

```sh
agyva profile init --local --endpoint openai --model gpt-4o   # alias: prof
agyva profile create work --account work --group work --warn-below 30 --default
agyva profile list
agyva profile which
agyva profile show work
agyva profile set work model claude-sonnet-4-20250514
agyva profile use work
agyva profile delete work -y
```

Resolution order without `--profile`: walk up from `cwd` for `.agyva.toml` (stops at `.git`, root, or 32 levels), then `config.default_profile`, then `default` in `profiles.toml`. `env` keys containing `API_KEY`, `SECRET`, `TOKEN`, `PASSWORD`, or `PASSPHRASE` are refused.

## Inspect quota and health

Only the active account refreshes live; others come from cache.

```sh
agyva usage                     # active account; normal refresh
agyva usage --all --compact     # compact table; non-active from cache
agyva usage work --no-refresh   # cache only
agyva statusline                # fast cache-only summary
agyva health --no-refresh       # health for all saved accounts
agyva watch --warn-below 25 --critical-below 10
```

| Flag | Effect |
| --- | --- |
| `--all` | All saved accounts (`usage`, `monitor`). |
| `--compact` | Single-line-per-account table (`usage`). |
| `--profile NAME` | Apply profile account/defaults (`usage`, `statusline`, `watch`, `monitor`). |
| `--no-refresh` | Cache only; never spawns `agy` (`usage`, `health`). |
| `--force-refresh` | Ignore `quota_min_refresh_secs` and check suspended account once (`usage`, `health`). |
| `--warn-below PCT`, `--critical-below PCT` | Threshold alerts (`watch`, `monitor`). |
| `--once` | Single cycle and exit (`watch`, `monitor`). |
| `--json` | JSON output ([JSON reference](json.md)). |
| `--json-stream` | NDJSON event stream (`watch`, `monitor`). |

Quota samples accumulate in `<cache>/quota-history.json` to compute burn rate (`%/h`) and exhaustion ETA. `watch` refreshes the active account at `watch_interval_secs`, backs off on transient errors, and optionally sends desktop notifications (`--notify` or `desktop_notifications = true`) without credentials.

### Statusline

Fast, cache-only summary without spawning `agy` or reading the keyring:

```sh
agyva statusline --format compact   # compact | full | plain | starship | tmux | waybar | json
```

### Model catalog

```sh
agyva models            # cache only
agyva models --refresh  # bounded `agy models` for active account (10s timeout)
agyva models --json
```

Cached in `<cache>/models.json` for `models_cache_secs` (86400s). `--refresh` respects the `agy` version gate (`--allow-untested` to bypass).

### Route-aware monitor

```sh
agyva monitor [--interval 15s] [--group NAME | --all] [--for-model FAMILY] [--profile NAME]
```

Alias: `agyva mon`. Evaluates cached evidence per tick (default `15s`, minimum `5s`) and refreshes the active account on a 300s single-flight cadence (plus `agy models` throttled by `models_cache_secs`). `--json` implies `--once` and prints one `MonitorSnapshot`; `--json-stream` emits NDJSON. Never switches unless opt-in auto-switch is enabled.

## Plan a route

```sh
agyva route init [-y]                            # create routing.toml with `default` group
agyva route add work --accounts work,personal    # --ranking smart|priority|least-used, --auto-switch
agyva route remove work [-y]
agyva route list
agyva route inspect GROUP [--for-model FAMILY]
agyva route plan [GROUP] [--for-model FAMILY] [--json]
agyva route apply GROUP [--yes] [--force]
agyva route history [--limit N]                  # 1-50, default 10
```

### Groups

`routing.toml` (beside `config.toml`):

```toml
[groups.work]
accounts = ["work", "personal"]
auto_switch = false
ranking = "smart"                # smart | priority | least-used
[groups.work.priorities]
work = 10
[groups.work.models]
"gemini-4-carbon" = "Gemini Pro"
[groups.work.plans]
work = "pro"                     # free | pro | ultra
```

| Field | Meaning |
| --- | --- |
| `accounts` | Member selectors (listed order breaks ties; empty = all saved accounts). |
| `ranking` | `smart` (default), `priority`, or `least-used`. |
| `priorities` | Integer priority per account (`priority` mode; higher first). |
| `plans` | User-declared `free`, `pro`, or `ultra` (never inferred). |
| `models` | Model family to quota group mapping for `--for-model`. |
| `auto_switch` | Opt-in automation (default `false`). |
| `five_hour_threshold_percent`, `weekly_threshold_percent` | Active quota trigger thresholds (0-100, default `10.0`). |
| `hysteresis_percent` | Minimum candidate lead over active account (0-100, default `10.0`). |
| `cooldown_secs` | Per-group switch cooldown override (30-86400s). |

### Inspect, plan, and --for-model

- `smart` ranks by tier, freshness, and reset timing; `priority` and `least-used` demote only ineligible accounts, then apply declared priority/plan.
- Stale, incomplete, reset-expired, unavailable, and suspended accounts are excluded (`-` shown in human tables).
- `--for-model` requires a fresh catalog (`agyva models`) and a mapped quota group (declared `[groups.<name>.models]` first, then group names).

### Apply and history

`route apply GROUP` runs one journaled switch, revalidated under lock. `--force` overrides evidence drift since planning, never pending recovery. `--json` requires `--yes`. `route history` prints bounded, credential-free decisions.

### Opt-in auto-switch

Off by default. Requires both `auto_switch = true` on the group and `agyva config set auto_switch_ack true`, and runs only inside `agyva monitor --group NAME`:

- Triggers only on fresh live active quota at/below threshold when a healthy standby leads by `hysteresis_percent` (candidate cache <= `auto_switch_candidate_max_age_secs`, default 21600s).
- Defers when `agy` sessions are active/uncertain, recovery is pending, or cooldown (`cooldown_secs` / `auto_switch_cooldown_secs`, default 900s; re-armed by manual `use`) has not elapsed.
- Records holds, defers, and switches in `route history`; cooldown state lives in `<state>/auto-switch.json` (0600).

## Recover an interrupted switch

```sh
agyva status          # alias: st
agyva doctor          # read-only diagnostics
agyva doctor --fix    # safe repairs: 0700/0600 modes, missing dirs, stale locks
agyva recover         # --rollback, --force
```

Exit 3 means recovery is pending. Resolve any keyring unlock prompt first, then run `agyva recover` (rolls forward if `Verified`, else rolls back; `--rollback` forces rollback). If `agy`'s live login matches neither side, `recover` refuses unless `--force` (which keeps a pinned backup first).

### Checkpoints

```sh
agyva checkpoints list                         # alias: ckpt
agyva checkpoints restore <NAME> [--force] [-y]
agyva checkpoints prune [--include-pinned] [-y]
```

`restore` refuses if recovery is pending (exit 3) or if live `agy` login differs without `--force` (which saves a pinned `*.keep.ckpt` backup first), and clears active custom endpoints in `endpoints.json`. `prune` trims ordinary `*.ckpt` files beyond `checkpoints_keep` (add `--include-pinned` for `*.keep.ckpt`).

## Move accounts between machines

```sh
agyva export /abs/path/accounts.agy [--account work]
agyva import /abs/path/accounts.agy
```

`export` requires an absolute path, never overwrites existing files, excludes the vault key and quota cache, and encrypts with a separate passphrase (`AGYVA_ARCHIVE_PASSPHRASE` or prompt, min 8 chars) using 64 MiB Argon2id (`import` accepts down to 19 MiB). `import` validates all entries upfront, skips existing identities untouched, refuses alias/id conflicts, and uses crash-safe staged publishing.

## Update Agyva

```sh
agyva update --check              # check GitHub Releases only
agyva update                      # download, verify SHA256SUMS + digest, replace binary
agyva update --install            # same as `agyva update`
```

Supported targets: `x86_64`/`aarch64` Linux (`gnu`), macOS, and Windows (`msvc`). Passive startup checks (2s timeout, cached 24h OK / 1h error in `<cache>/update-check.json`) can be disabled via `check_updates = false` or `AGYVA_UPDATE_CHECK=0`.

## Rekey the vault

```sh
agyva rekey                      # rotate key/passphrase in current mode
agyva rekey --mode passphrase    # migrate to passphrase (AGYVA_NEW_PASSPHRASE or prompt)
agyva rekey --mode keyring       # migrate to OS keyring
```

Re-encrypts all snapshots and checkpoints atomically before committing `vault.json`. Blocked by pending recovery (exit 3).

## Settings

`agyva config show | set <key> <value> | reset <key> | path`

| Setting | Default / values |
| --- | --- |
| `allow_untested_agy` | `false`; accept `agy` versions outside `1.5.x`. |
| `auto_switch_ack` | `false`; explicit opt-in ack for route auto-switch. |
| `auto_switch_candidate_max_age_secs` | `21600` (60-604800); max cache age for standby candidates. |
| `auto_switch_cooldown_secs` | `900` (30-86400); min seconds between switches. |
| `backend` | `auto`; `secret-service` (aliases `keyring`, `keychain`, `credential-manager`) or `file`. |
| `check_updates` | `true`; startup GitHub Releases check. |
| `checkpoints_keep` | `5` (1-20); ordinary checkpoints kept. |
| `color` | `auto`; `always`, `never`. |
| `critical_percent` | `10` (0-100, `<= warn_percent`); critical quota alert threshold. |
| `default_profile` | unset; default global profile name (empty string unsets). |
| `desktop_notifications` | `false`; allow `watch` desktop notifications. |
| `lock_timeout_secs` | `10` (0-600); lock wait timeout. |
| `models_cache_secs` | `86400` (60-604800); model catalog freshness TTL. |
| `policy_acknowledged` | `false`; one-time policy notice state. |
| `quota_fresh_secs` | `300` (10-86400); quota freshness TTL. |
| `quota_min_refresh_secs` | `60` (10-3600); minimum interval between refreshes. |
| `statusline_format` | `compact`; `compact`, `full`, or `json`. |
| `vault_key` | `auto`; `keyring` or `passphrase`. |
| `warn_percent` | `25` (0-100); warning quota alert threshold. |
| `watch_interval_secs` | `300` (60-86400); `watch` polling interval. |

`AGYVA_LOG=debug` logs to stderr (disabled in TUI). Development env vars: [environment overrides](development.md#environment-overrides).

## Shell completion

```sh
agyva completions fish > ~/.config/fish/completions/agyva.fish
agyva completions bash > ~/.local/share/bash-completion/completions/agyva
agyva completions zsh > ~/.zfunc/_agyva
agyva completions powershell > agyva.ps1
agyva completions elvish > agyva.elv
```

Fish completions also resolve saved aliases, `ep:<endpoint>`, profiles, and checkpoints via read-only `--json` calls (no keyring access).
