# agyva

[![CI](https://github.com/0x1d1e/agyva/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/0x1d1e/agyva/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/github/license/0x1d1e/agyva.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/0x1d1e/agyva?include_prereleases&sort=semver)](https://github.com/0x1d1e/agyva/releases)

Save your Google Antigravity CLI (`agy`) logins, inspect their cached quota,
and choose which account to use. Agyva keeps encrypted snapshots and restores
the login you select. One Rust binary, a terminal dashboard, no daemon.
Switching is manual unless you explicitly opt in to `route` automation with
`auto_switch = true` **and** `agyva config set auto_switch_ack true`; even
then it is bounded by a 15-minute cooldown, fresh evidence and a session
gate, and every decision is journaled. Restart `agy` after each switch.

**Unofficial, Linux only.** Google’s [Antigravity FAQ](https://antigravity.google/docs/faq/)
says third-party tools and account rotation are not supported on consumer
plans. Read the [policy and security model](docs/security.md) before saving
accounts. Agyva does not bypass limits or work around suspensions.

![Actual Agyva Usage rendering: stale cached Gemini weekly 99 percent, Claude and GPT zero percent, five-hour windows not reported](docs/tui-usage.svg)

Captured from the installed Agyva’s existing account registry and quota cache;
identifying fields replaced with samples. Quotas and timestamps preserved.
No refresh or live login verification performed.

This is the production TUI renderer reading an isolated replay of that cache.
The [Accounts overview](docs/tui.svg) shows the **minimum reported remaining
quota across enabled groups**, not an account-wide allowance.
[All captured views](docs/tui-capture.txt) · [Capture provenance](docs/dashboard.md#capture-provenance).

## Install

These sources build `v0.1.1`. Release artifacts are cut by pushing a `v*` tag
(see [`release.yml`](.github/workflows/release.yml)); when the tag is published,
download the Linux x86-64 archive and `SHA256SUMS` from its release page — the
currently published release is
[v0.1.0](https://github.com/0x1d1e/agyva/releases/tag/v0.1.0). In the download
directory:

```sh
sha256sum -c SHA256SUMS
tar -xzf agyva-0.1.0-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 agyva-0.1.0-x86_64-unknown-linux-gnu/agyva ~/.local/bin/agyva
```

Or install the current source with Rust 1.88 or newer:

```sh
git clone https://github.com/0x1d1e/agyva.git
cd agyva
cargo install --path . --locked
```

Put `~/.local/bin` or `~/.cargo/bin` on your `PATH`. Agyva does not edit shell
configuration. Install `agy` separately. The supported version gate is 1.5.x;
the consumer credential and quota shapes were observed on 1.5.9. **Writing
agy’s Secret Service item remains unverified against real agy.** See the
[compatibility matrix](docs/compatibility.md) before switching.

The default Linux store needs a Secret Service provider such as GNOME
Keyring, KeePassXC or KWallet’s bridge. Headless and token-file setups are
covered in [Antigravity setup](docs/antigravity.md#choose-a-credential-store).

## Save, inspect, switch

Log in with `agy`, then save that login. Repeat after signing in to your
second account with `agy`:

```sh
agyva add --alias personal
# Sign in to your second account with agy, then:
agyva add --alias work

agyva list
agyva usage --all --no-refresh   # inspect the cache without running agy
agyva use personal              # explicit manual switch
# Restart agy so it uses the selected login.
agyva usage                     # read quota for the active account
agyva                           # open the terminal dashboard
```

`add` saves the login that `agy` already holds. It does not sign you in.
`use` accepts an alias, list number, exact email, account id or unique prefix;
`agyva use -` returns to the previous account. Running `agy` sessions keep
their old login until restarted.

## Common tasks

**See remaining quota.** `agyva usage --all` refreshes only the active account;
others show their last cache reading. Each group keeps its own five-hour and
weekly windows, reset times and source. Missing windows say `not reported`.
Cached health is not live login verification. [Quota and health](docs/antigravity.md#read-quota-and-health).

**Use the dashboard.** `Tab` changes view, `↑↓` selects, `Enter` opens switch
confirmation, `r` refreshes the active account, `d` opens details, `/` filters,
`?` opens help and `q` quits. Help and Details scroll on short terminals.
[Dashboard and keys](docs/dashboard.md).

**Recover an interrupted switch.** Start with `agyva status` and
`agyva doctor`. Exit code 3 means recovery is required: resolve any keyring
unlock prompt, then run `agyva recover`. [Recovery and troubleshooting](docs/commands.md#recover-an-interrupted-switch).

**Script an inspection.** `agyva list --json` reads the public registry;
`agyva usage --all --no-refresh --json` reads cached quota. JSON schemas and
exit codes stay stable independently of terminal wording. [JSON and streams](docs/json.md).

**Configure or complete commands.** `agyva config show` explains each setting.
`agyva completions fish`, `bash` or `zsh` prints a completion script.
[Commands and settings](docs/commands.md).

## Security and limits

Saved logins and pre-switch checkpoints use AES-256-GCM. The vault key lives
in the OS keyring or is derived from a passphrase with Argon2id. Switches are
journaled, read back and verified locally. Encryption protects saved copies
at rest; it does not protect an unlocked, compromised user session.

Quota comes from `agy -p "/usage" --output-format json`. The only other
network traffic is the optional self-update: one HTTPS request to this
project's GitHub Releases on every enabled startup, including JSON, completions
and TUI invocations (silent; disable with `check_updates = false` or
`AGYVA_UPDATE_CHECK=0`), plus the download performed by an explicit
`agyva update`. Only the *delivery* of the notice is suppressed for JSON,
completions and the TUI, which shows it on its dashboard; the request itself
still happens. No telemetry, no other endpoints. Quota never switches because
of an error and suspended accounts are never retried automatically. The JSON
envelope is locally observed and undocumented, so changed shapes are reported
as unavailable. Plans are not reported. macOS and Windows are unsupported.

Accounts can move between machines: `agyva export` writes a passphrase-
encrypted archive, `agyva import` adds what is missing and never touches what
is already saved. `agyva update --check` reports a newer release;
`agyva update` verifies the release checksums and installs it atomically.

The [security model](docs/security.md) covers checkpoint retention, the
residual switch race, old encrypted blob rollback, passphrase handling and
policy. The [compatibility matrix](docs/compatibility.md) separates enforced,
observed and unverified behavior, including keyring writes.

## Documentation

- [Commands, settings and recovery](docs/commands.md)
- [Dashboard, keys, resize and capture provenance](docs/dashboard.md)
- [Antigravity setup, quota windows and health](docs/antigravity.md)
- [JSON output and exit codes](docs/json.md)
- [Security and policy](docs/security.md) · [Compatibility](docs/compatibility.md)
- [Development and isolated captures](docs/development.md) · [Architecture](docs/architecture.md)
- [Contributing](CONTRIBUTING.md) · [Changelog](CHANGELOG.md) · [Security reports](SECURITY.md)

## License

MIT. See [LICENSE](LICENSE).
