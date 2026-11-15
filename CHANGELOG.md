# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Agyva is at `0.1.x`. Anything in `Unreleased` is on `main` but has not been
tagged; release artifacts are built by pushing a `v*` tag (see
[`.github/workflows/release.yml`](.github/workflows/release.yml)).

## [Unreleased]

### Fixed

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

## [0.1.0] - 2026-11-15

First version. Not yet tagged when this entry was written.

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
