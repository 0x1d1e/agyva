# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Agyva is at `0.1.x`. Anything in `Unreleased` is on `main` but has not been
tagged; release artifacts are built by pushing a `v*` tag (see
[`.github/workflows/release.yml`](.github/workflows/release.yml)).

## [Unreleased]

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

### Fixed

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
