# agyva

[![CI](https://github.com/0x1d1e/agyva/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/0x1d1e/agyva/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/github/license/0x1d1e/agyva.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/0x1d1e/agyva?include_prereleases&sort=semver)](https://github.com/0x1d1e/agyva/releases)

Account, quota, custom endpoint, and profile manager for Google's Antigravity CLI (`agy`).

**Unofficial.** Targets Linux, macOS, and Windows (x64 and ARM64). Linux is tested locally; macOS Keychain and Windows Credential Manager are implemented and CI-configured, unverified locally on hardware. Google's [Antigravity FAQ](https://antigravity.google/docs/faq/) states third-party tools and account rotation are unsupported on consumer plans. Read the [security model](docs/security.md) first: `agyva` does not bypass limits or suspensions.

![Actual Agyva Usage rendering: stale cached Gemini weekly 99 percent, Claude and GPT zero percent, five-hour windows not reported](docs/tui-usage.svg)

Rendered from sanitized real cache data (no live verification). The [Accounts overview](docs/tui.svg) shows the **minimum remaining quota across enabled groups**, not a shared account allowance. [All views](docs/tui-capture.txt) · [Provenance](docs/dashboard.md#capture-provenance).

## Contents

- [Features](#features) · [Requirements](#requirements) · [Install](#install) · [Quick start](#quick-start)
- [Routing and auto-switch](#routing-and-auto-switch) · [Tasks](#tasks) · [Security and limits](#security-and-limits)
- [Documentation](#documentation) · [License](#license)

## Features

- **Account vault:** Saves existing `agy` logins in AES-256-GCM snapshots and switches via a crash-safe journal with pre-switch checkpoints (`agyva recover`, `agyva checkpoints`).
- **Quota and burn-rate ETA:** Reads `agy -p "/usage" --output-format json`, caches per-group windows and health, and computes burn-rate ETAs from `<cache>/quota-history.json`.
- **5-tab TUI:** Interactive dashboard (`Accounts`, `Usage`, `Endpoints`, `Routes`, `Diagnostics`) driven by the same core data as the CLI.
- **Custom endpoints and bridge:** Encrypted API keys, 17 provider presets, `auto` protocol detection, model/rate-limit discovery, `agy` `settings.json` sync, a loopback HTTP/1.1 protocol bridge (`agyva bridge`), shell exports (`agyva env`), and wrapped execution (`agyva exec`).
- **Profiles and statusline:** Project-local `.agyva.toml` and global `profiles.toml` defaults (`agyva profile`), plus cache-only prompts (`agyva statusline`).
- **Routing:** `routing.toml` groups ranked by `smart`, `priority`, or `least-used`, filterable with `--for-model`, plus doubly gated opt-in auto-switch via `agyva monitor`.
- **Portable transfer and updates:** Passphrase-encrypted `export`/`import` archives and checksum-verified GitHub Releases updates.

## Requirements

- `agy` `1.5.x` installed and signed in separately (`agyva` does not sign in for you).
- Credential store: OS keyring (Linux Secret Service, macOS Keychain, Windows Credential Manager), native token file (`backend = file`), or passphrase vault (`vault_key = passphrase`). See [Antigravity setup](docs/antigravity.md#choose-a-credential-store).
- Building from source requires Rust 1.88+.

The compatibility gate accepts `agy` `1.5.x` (observed on `1.5.9`). **Keyring write compatibility with real `agy` remains unverified**; see [compatibility](docs/compatibility.md).

## Install

Linux (`x86_64`, `aarch64`) and macOS (`x86_64`, `aarch64`) via [`install.sh`](install.sh):

```sh
curl -fsSL https://raw.githubusercontent.com/0x1d1e/agyva/main/install.sh | sh
```

The script verifies the raw binary against `SHA256SUMS` and installs to `~/.local/bin/agyva`. Options via `sh -s --`: `--version <vX.Y.Z>`, `--prefix <dir>`, `--uninstall`.

Or download the raw binary and `SHA256SUMS` for your target from [GitHub Releases](https://github.com/0x1d1e/agyva/releases) (no `.tar.gz` archives; 6 targets for Linux, macOS, and Windows x64/ARM64):

```sh
sha256sum -c SHA256SUMS
install -Dm755 agyva-0.1.3-x86_64-unknown-linux-gnu ~/.local/bin/agyva
```

Or build from source:

```sh
git clone https://github.com/0x1d1e/agyva.git && cd agyva
cargo install --path . --locked
```

Ensure the install directory is on your `PATH`.

## Quick start

```sh
agyva add --alias personal      # save agy's current login
agyva list                      # list saved accounts
agyva usage --all --no-refresh  # inspect cached quota
agyva use personal              # manual switch (restart agy afterward)
agyva endpoint add openai --preset openai --api-key-env OPENAI_API_KEY
agyva use ep:openai             # point agy settings at a custom endpoint
agyva exec --endpoint openai -- my-tool
agyva                           # open the 5-tab terminal dashboard
```

`use` accepts an alias, list number, exact email, account id or unique prefix, `ep:<name>`, `--profile <name>`, or `-` (previous account). **Restart `agy` after every switch**: running sessions keep their startup login.

## Routing and auto-switch

`routing.toml` (beside `config.toml`) defines groups, rankings (`smart`, `priority`, `least-used`), declared plans, and model-to-quota-group mappings:

- `route init|add|remove|list`: manage groups.
- `route inspect|plan GROUP [--for-model FAMILY]`: rank cached evidence without running `agy`.
- `route apply GROUP [--yes]`: run one journaled switch (`--force` skips evidence-drift check, not pending recovery).
- `route history [--limit N]`: view recent routing decisions.
- `agyva monitor [--group NAME]`: re-rank cached evidence on an interval with burn-rate ETAs.

Auto-switch is off by default and requires both `auto_switch = true` on the group and `agyva config set auto_switch_ack true`. Only `agyva monitor --group NAME` drives it, subject to cooldown (`auto_switch_cooldown_secs`, default 900s), candidate max age (`auto_switch_candidate_max_age_secs`, default 21600s), thresholds/hysteresis (default 10%), fresh live evidence, and no active `agy` sessions. See [Opt-in auto-switch](docs/commands.md#opt-in-auto-switch).

## Tasks

| Task | Commands | Details |
| --- | --- | --- |
| Quota and health | `agyva usage --all`, `agyva health --no-refresh`, `agyva models --refresh` | [Antigravity setup](docs/antigravity.md#read-quota-and-health) |
| Custom endpoints | `agyva endpoint presets\|add\|list\|test\|use`, `bridge`, `env`, `exec` | [Endpoints](docs/commands.md#custom-endpoints-bridge-env-and-exec) |
| Profiles and statusline | `agyva profile init\|list\|which\|use`, `agyva statusline` | [Profiles](docs/commands.md#project-and-global-profiles) |
| Terminal dashboard | `agyva` (`Tab`/`F1`-`F5`, `Enter`, `s`, `t`/`f`, `r`, `d`, `/`, `?`, `q`) | [Dashboard](docs/dashboard.md#keys) |
| Diagnostics and recovery | `agyva status`, `agyva doctor [--fix]`, `agyva checkpoints`, `agyva recover` | [Recovery](docs/commands.md#recover-an-interrupted-switch) |
| Scripting | `agyva list --json`, `agyva usage --all --json`, `agyva monitor --json-stream` | [JSON and exit codes](docs/json.md) |
| Config and completions | `agyva config show\|set\|reset\|path`, `agyva completions <shell>` | [Settings](docs/commands.md#settings) |
| Vault rekey and transfer | `agyva rekey`, `agyva export FILE`, `agyva import FILE` | [Commands](docs/commands.md#rekey-the-vault) |
| Self-update | `agyva update --check`, `agyva update` | [Update](docs/commands.md#update-agyva) |

## Security and limits

- **At-rest encryption:** Accounts, endpoint secrets (`endpoints/<id>.ep.snap`), and checkpoints use AES-256-GCM envelopes keyed from the OS keyring or Argon2id passphrase. Export archives use a separate passphrase (`AGYVA_ARCHIVE_PASSPHRASE`, min 8 chars) with 64 MiB Argon2id (`import` accepts down to 19 MiB); vault keys are never exported.
- **Journaled switches:** Switches checkpoint, write, read back, and verify locally. Exit code 3 means recovery is pending: resolve any keyring prompt and run `agyva recover`.
- **Quota boundaries:** Quota comes only from `agy -p "/usage" --output-format json`. Unrecognized envelopes become unavailable; suspended accounts are never retried automatically.
- **Network surfaces:** Only user-configured endpoints (`https://`, or loopback/local `http://`) and optional GitHub Releases self-update on the 6 supported targets (`<cache>/update-check.json`, 24h OK / 1h error TTL). Disable startup checks with `check_updates = false` or `AGYVA_UPDATE_CHECK=0`. No telemetry.
- **Threat boundary:** Encryption protects saved files at rest, not an unlocked user session. See [security](docs/security.md) and [compatibility](docs/compatibility.md).

## Documentation

| Document | Contents |
| --- | --- |
| [Commands](docs/commands.md) | CLI commands, flags, settings, recovery |
| [Dashboard](docs/dashboard.md) | 5-tab TUI views, keys, capture provenance |
| [Antigravity setup](docs/antigravity.md) | Credential stores, quota windows, health, restarts |
| [JSON and exit codes](docs/json.md) | `--json` schemas and exit codes |
| [Security and policy](docs/security.md) | Threat model, encryption, switch safety, limits |
| [Compatibility](docs/compatibility.md) | Enforced, observed, and unverified `agy` behavior |
| [Architecture](docs/architecture.md) | Modules, storage layout, switch internals |
| [Development](docs/development.md) | Build, tests, env overrides, scripts |
| [Contributing](CONTRIBUTING.md) · [Changelog](CHANGELOG.md) · [Security](SECURITY.md) | Contribution rules, release notes, vulnerability reporting |

## License

MIT. See [LICENSE](LICENSE).
