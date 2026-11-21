# agyva

[![CI](https://github.com/0x1d1e/agyva/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/0x1d1e/agyva/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/github/license/0x1d1e/agyva.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/0x1d1e/agyva?include_prereleases&sort=semver)](https://github.com/0x1d1e/agyva/releases)

Manual account and endpoint manager for Google's Antigravity CLI (`agy`).

Agyva saves the logins `agy` already holds, reads their cached quota, manages custom LLM endpoints and project profiles, and restores the target you pick. Snapshots stay encrypted, every switch is journaled and verified, and the same commands drive a 5-tab terminal dashboard.

**Unofficial.** Targets Linux, macOS, and Windows (x64 and ARM64; macOS Keychain and Windows Credential Manager are implemented and CI-configured, unverified locally on macOS/Windows hardware). Google’s [Antigravity FAQ](https://antigravity.google/docs/faq/) says third-party tools and account rotation are not supported on consumer plans. Read the [policy and security model](docs/security.md) first; Agyva does not bypass limits or work around suspensions.

![Actual Agyva Usage rendering: stale cached Gemini weekly 99 percent, Claude and GPT zero percent, five-hour windows not reported](docs/tui-usage.svg)

Real cache, sample identities, quotas and timestamps preserved, no live verification. The [Accounts
overview](docs/tui.svg) shows the **minimum remaining quota across enabled groups**, not an account-wide allowance.
[All views](docs/tui-capture.txt) · [Provenance](docs/dashboard.md#capture-provenance).

## Table of Contents

- [What it does](#what-it-does) · [Requirements](#requirements) · [Install](#install) · [Quick start](#quick-start)
- [Routing](#routing) · [Common tasks](#common-tasks) · [Security and limits](#security-and-limits)
- [Documentation](#documentation) · [License](#license)

## What it does

- Saves the login `agy` already holds as an encrypted snapshot and restores it.
- Caches quota per account (five-hour and weekly windows, resets, freshness) and computes burn-rate ETA from `<cache>/quota-history.json` without inventing numbers.
- Drives a 5-tab terminal dashboard: Accounts, Usage, Endpoints, Routes, Diagnostics, with summary cards, smooth gauges, sorting, filtering, and help.
- Manages custom LLM endpoints (`agyva endpoint`): encrypted API keys, 17 provider presets, `auto` protocol detection, model/rate-limit discovery, `agy` `settings.json` sync, a local HTTP/1.1 protocol bridge (`agyva bridge`), shell exports (`agyva env`), and wrapped execution (`agyva exec`).
- Manages project-local `.agyva.toml` and global `profiles.toml` profiles (`agyva profile`), plus cache-only `agyva statusline` prompts.
- Journals, checkpoints and verifies every switch; `agyva recover` finishes or rolls back a broken one, and `agyva checkpoints` lists, restores, or prunes saved checkpoints.
- Plans routes from `routing.toml` groups (`route init|add|remove|list|inspect|plan|apply|history`) by tier, priority or least-used plan, filtered by `--for-model`.
- Opts in to auto-switch per group, doubly gated and journaled, driven only by `agyva monitor --group NAME`.
- Moves accounts between machines with passphrase-encrypted `export`/`import`.
- Updates from GitHub Releases after checksum verification, with no telemetry.

## Requirements

Prebuilt releases target Linux (`x86_64`, `aarch64`), macOS (`x86_64`, `aarch64`), and Windows (`x86_64`, `aarch64` MSVC). Linux behavior is tested locally; macOS Keychain and Windows Credential Manager backends are implemented and CI-configured, unverified locally on macOS or Windows hardware.

Install `agy` 1.5.x and sign in separately; Agyva does not sign you in. The default store uses the OS keyring (freedesktop Secret Service on Linux, Apple Keychain on macOS, Windows Credential Manager on Windows), the native token file (`backend = file`), or the passphrase vault (`vault_key = passphrase`); see [Antigravity setup](docs/antigravity.md#choose-a-credential-store). Building from source needs Rust 1.88 or newer.

## Install

Install the latest release on Linux (x86-64 / ARM64) or macOS (x86-64 / ARM64) with [`install.sh`](install.sh):

```sh
curl -fsSL https://raw.githubusercontent.com/0x1d1e/agyva/main/install.sh | sh
```

The installer detects OS and architecture, verifies the release binary against `SHA256SUMS`, and installs into `~/.local/bin/agyva`. Pass options via `sh -s --`: `--version <vX.Y.Z>`, `--prefix <dir>`, or `--uninstall`.

Or download a standalone release binary and `SHA256SUMS` from a `v*` release ([`release.yml`](.github/workflows/release.yml)) and verify manually:

```sh
sha256sum -c SHA256SUMS
install -Dm755 agyva-0.1.3-x86_64-unknown-linux-gnu ~/.local/bin/agyva
```

Or build this tree:

```sh
git clone https://github.com/0x1d1e/agyva.git && cd agyva
cargo install --path . --locked
```

Put the binary directory on your `PATH`; Agyva does not edit shell configuration. This tree builds `0.1.3`; release tags are published on [GitHub Releases](https://github.com/0x1d1e/agyva/releases). The gate accepts `agy` 1.5.x, shaped from 1.5.9 observations, and **keyring writes remain unverified against real `agy`** — see [compatibility](docs/compatibility.md).

## Quick start

Log in with `agy` and save that login; repeat per account:

```sh
agyva add --alias personal      # save the login agy already holds
agyva list                      # saved accounts, and which is active
agyva usage --all --no-refresh  # cached quota, without running agy
agyva use personal              # explicit manual switch
agyva endpoint add openai --preset openai --api-key-env OPENAI_API_KEY
agyva use ep:openai             # switch agy settings to a custom endpoint
agyva exec --endpoint openai -- my-tool
agyva                           # open the 5-tab terminal dashboard
```

`add` saves the login `agy` already holds; it does not sign you in. `use` accepts an alias, list number, exact email, account id or unique prefix, `ep:<name>` for custom endpoints, `--profile <name>`, or `-` for the previous account. **Restart `agy` after each switch**: running sessions keep the login they started with.

## Routing

Groups in `routing.toml`, beside `config.toml`, say which accounts belong together, how they rank, and which quota group pays for a model family.

- `route init` seeds a `default` group from saved accounts; `route add|remove` edit groups; `route list` names each group; `route inspect|plan GROUP` rank cached evidence without running agy; `--for-model FAMILY` keeps accounts whose cached catalog offers that family.
- `route apply GROUP` makes one journaled switch, refusing if evidence moved since planning (`--force` skips that drift check, never a pending recovery); `route history` lists the decisions.
- `agyva monitor [--group NAME]` (`mon`) re-ranks cached evidence on an interval, tracks burn-rate ETA, and supports `--once`, `--json`, `--json-stream`, `--profile`, and threshold flags; `agyva models --refresh` runs `agy models` once for the active account.

Auto-switch is off by default and doubly gated: the group sets `auto_switch = true` **and** you run `agyva config set auto_switch_ack true`. Only `agyva monitor --group NAME` drives it — configurable cooldown (`auto_switch_cooldown_secs`, default 900s, fail-closed cooldown state), candidate age (`auto_switch_candidate_max_age_secs`, default 21600s), active thresholds and hysteresis (default 10%), fresh-evidence and session gates, every decision journaled. See [Plan a route](docs/commands.md#plan-a-route) and [Opt-in auto-switch](docs/commands.md#opt-in-auto-switch).

## Common tasks

| Task | Command · where |
| --- | --- |
| See remaining quota | `agyva usage --all`; only the active account refreshes, others show their cache · [Quota](docs/antigravity.md#read-quota-and-health) |
| Manage custom endpoints & bridge | `agyva endpoint presets\|add\|list\|test\|use`, `agyva bridge`, `agyva env`, `agyva exec` · [Endpoints](docs/commands.md#custom-endpoints-bridge-env-and-exec) |
| Use project/global profiles & statusline | `agyva profile init\|list\|which\|use`, `agyva statusline [--format compact\|full\|starship\|tmux\|waybar\|json]` · [Profiles](docs/commands.md#project-and-global-profiles) |
| Drive the dashboard | `agyva`, then `Tab`/`F1`–`F5` view, `↑↓` select, `Enter` activate, `s` sort, `t`/`f` probe/fetch, `r` refresh, `d` details, `/` filter, `?` help, `q` quit · [Keys](docs/dashboard.md#keys) |
| Manage checkpoints & recover | `agyva status`, `agyva doctor [--fix]`, `agyva checkpoints list\|restore\|prune`, `agyva recover` · [Recovery](docs/commands.md#recover-an-interrupted-switch) |
| Script an inspection | `agyva list --json`, `agyva usage --all --no-refresh --json`, `agyva monitor --json-stream` · [JSON](docs/json.md) |
| Change settings, add completions | `agyva config show\|set\|reset\|path`, `agyva completions fish\|bash\|zsh\|powershell\|elvish` · [Settings](docs/commands.md#settings) |
| Rotate vault key or migrate mode | `agyva rekey [--mode keyring\|passphrase]` · [Rekey](docs/commands.md#rekey-the-vault) |
| Move accounts between machines | `agyva export FILE`, `agyva import FILE`; adds missing accounts, never alters saved ones · [Transfer](docs/commands.md#move-accounts-between-machines) |
| Update Agyva | `agyva update --check` reports; `agyva update` verifies and installs · [Update](docs/commands.md#update-agyva) |

## Security and limits

- Saved logins, custom endpoint secrets (`endpoints/<id>.ep.snap`), and checkpoints are AES-256-GCM envelopes; the vault key lives in the OS keyring or comes from a passphrase via Argon2id. Export archives are sealed under a separate passphrase (`AGYVA_ARCHIVE_PASSPHRASE`, minimum 8 characters) using 64 MiB Argon2id (`import` accepts down to the 19 MiB floor); the vault key is never exported.
- Switches are journaled, read back and verified locally. Exit 3 means recovery is required: resolve any keyring unlock prompt, then run `agyva recover`.
- Quota comes only from `agy -p "/usage" --output-format json`; that envelope is locally observed and undocumented, so a changed shape reads as unavailable. Quota never switches because of an error, and suspended accounts are never retried.
- No telemetry. Network requests occur only for user-configured custom endpoints (`endpoint add`, `test`, `models --refresh`, `bridge`, `exec --bridge`; HTTPS required except loopback/local HTTP) and optional self-update: one HTTPS request to this project’s GitHub Releases on supported targets (`x86_64`/`aarch64` Linux, macOS, Windows MSVC), cached in `<cache>/update-check.json` (24 hours on success, 1 hour on failure) and silent on failure. `check_updates = false` or `AGYVA_UPDATE_CHECK=0` disables the startup check; `--json` and `completions` suppress the notice, and in the TUI it appears on the dashboard instead of stderr.
- Encryption protects saved copies at rest, not an unlocked, compromised session. Details: [security model](docs/security.md) · [compatibility matrix](docs/compatibility.md).

## Documentation

| Document | What it answers |
| --- | --- |
| [Commands, settings and recovery](docs/commands.md) | Every command, flag, setting and recovery step |
| [Dashboard](docs/dashboard.md) | Views, keys, narrow terminals and capture provenance |
| [Antigravity setup](docs/antigravity.md) | Credential stores, quota windows, health, restarts |
| [JSON and exit codes](docs/json.md) | The scripting contract, field by field |
| [Security and policy](docs/security.md) | Threat model, encryption, switch safety, limits |
| [Compatibility](docs/compatibility.md) | Enforced, observed and unverified `agy` behavior |
| [Architecture](docs/architecture.md) | Modules, storage layout and switch internals |
| [Development](docs/development.md) | Build, tests, environment overrides and captures |
| [Contributing](CONTRIBUTING.md) · [Changelog](CHANGELOG.md) · [Security reports](SECURITY.md) | How to help, what changed, how to report a vulnerability |

## License

MIT. See [LICENSE](LICENSE).
