# Security and policy

[README](../README.md) · [Commands](commands.md) · [Compatibility](compatibility.md) · [JSON and exit codes](json.md)

Agyva is a local, manual account switcher: it moves agy's own stored login
between encrypted local slots and nothing else.

## Table of Contents

- [Policy](#policy)
- [What is protected](#what-is-protected)
- [Usage, health and the TUI](#usage-health-and-the-tui)
- [Switch safety](#switch-safety)
- [Known limits](#known-limits)
- [Unverified compatibility](#unverified-compatibility)
- [Compatibility policy](#compatibility-policy)
- [Reporting](#reporting)

## Policy

Agyva is a **manual** account switcher by default. It switches only when you
run `agyva use` or confirm a dashboard switch. It does not rotate, balance or
auto-switch accounts **unless you explicitly opt in** (see below), does not
retry on quota errors, and is not a way to work around usage limits. You are
responsible for complying with Google's terms for every account you save. The
first `add` or `use` prints this notice once (recorded as
`policy_acknowledged` in `config.toml`).

Google’s [Antigravity FAQ](https://antigravity.google/docs/faq/) says using
third-party tools or rotating accounts is not supported on consumer plans.
Accounts can be suspended; Agyva cannot prevent that or work around it.

Agyva does not use its own OAuth client, private Google endpoints, or a
spoofed user agent. It only moves agy's own stored login between local slots.

After a switch, **restart agy**: running sessions keep their old login. Agyva
never terminates agy; it only reports detected sessions (from
`presence/*.lock` files and the process list).

## What is protected

Saved credentials and checkpoints are sealed at rest, and the surrounding files
are written defensively:

- **Envelope.** Saved credentials, custom endpoint secrets (`endpoints/<id>.ep.snap`),
  and checkpoints are AES-256-GCM envelopes: a versioned header, a random 96-bit
  nonce, and AAD binding the format version, algorithm and a context string
  (`snapshot:<id>`, `endpoint:<id>`, `checkpoint:<txn>`). A file moved to
  another slot, a flipped bit or a changed version fails to open.
- **Vault key.** It lives in the OS keyring (`agyva`/`vault-key`). If no keyring
  is reachable, or `vault_key = passphrase`, the key is derived with Argon2id
  (19 MiB, t=2, p=1 by default) from a passphrase of at least 8 characters
  taken from a TTY prompt or `AGYVA_PASSPHRASE`; never from argv. `agyva rekey`
  rotates the key or passphrase, or migrates between keyring and passphrase
  modes, re-encrypting all snapshots and checkpoints atomically before
  committing `vault.json` (using `AGYVA_NEW_PASSPHRASE` or a prompt for the new
  passphrase). KDF parameters read back from `vault.json` are
  bounds-checked (memory 8 KiB to 1 GiB, t and p 1 to 16, salt 8 to 64 bytes);
  anything outside is treated as a corrupt vault, so a tampered file cannot
  demand absurd memory or time.
- **No plaintext residue.** No plaintext key file, no plaintext credential
  copies, no `.bak` files.
- **Filesystem discipline.** Files are created 0600 and directories 0700; writes
  are atomic (temp file, fsync, rename, directory fsync).
- **Passphrase hygiene.** `AGYVA_PASSPHRASE`, `AGYVA_NEW_PASSPHRASE` and
  `AGYVA_ARCHIVE_PASSPHRASE` are moved out of the environment map into
  zeroizing holders at startup and are never passed to child processes
  (`agy` runs with all `AGYVA_*` variables removed). They cannot be removed
  from the process's own environment without `unsafe` code, which the project
  does not use; prefer the interactive prompt.
- **Ids from disk.** Account ids and checkpoint names read from disk (registry,
  journal) are validated (10 lowercase hex characters; a fixed name shape)
  before any file path is built from them.
- **Fenced test hooks.** `AGYVA_BACKEND` (which swaps agy's credential store and
  seals the keyring) is honored only when the config, state and agy-home
  directories are redirected too; `AGYVA_FAILPOINT` only with
  `AGYVA_TEST_MODE=1`.
- **No secrets in output.** Secrets are held in zeroizing wrappers with redacted
  `Debug`. Keyring errors are mapped by variant so their text cannot echo secret
  bytes. JSON output and error messages never include credential material.

## Usage, health and the TUI

Everything Agyva reads or reports about quota, health and the dashboard stays
local, read-only and credential-free, with self-update and user-configured
custom endpoints as the only network surfaces.

| Surface | What it does | What it never does |
| --- | --- | --- |
| Quota | Runs agy's own `/usage` for the active login: no stdin, all `AGYVA_*` variables removed, bounded output, time-limited. | Make a network request of its own, use an OAuth client of its own, or pass a credential to agy. |
| Quota cache (`<cache>/quota.json`, 0600) | Holds percentages, reset times, health state and a one-line rule description. | Hold tokens, email or raw agy output. The TUI and JSON show the same data, and tests assert none of the synthetic secrets appear in the cache or output. |
| Custom endpoints & bridge | Encrypts API keys in `endpoints/<id>.ep.snap`; enforces `https://` (or loopback/local `http://`), rejects embedded URL userinfo and `169.254/16` link-local targets, binds `bridge` to loopback only, and redacts secrets from probe/bridge errors. | Write API keys to `endpoints.json`, `settings.json`, `endpoint-models.json`, or `--json` output. |
| Profiles (`.agyva.toml`, `profiles.toml`) | Stores non-secret defaults (`account`, `endpoint`, `group`, `model`, thresholds, non-secret `env`). | Accept `env` keys containing `API_KEY`, `SECRET`, `TOKEN`, `PASSWORD`, or `PASSPHRASE`. |
| Self-update | See [below](#self-update): HTTPS GETs to GitHub Releases, verified and staged atomically. | Touch any other host, or treat a malformed digest as absent. |
| Export archives (`core::transfer`) | Sealed with AES-256-GCM under the dedicated `archive:1` context, keyed from a **separate** archive passphrase via Argon2id (64 MiB, `t=3`, `p=1` on export; import accepts down to the 19 MiB, `t=2`, `p=1` floor). | Export the vault key, or modify existing identities on import (they are skipped bit-for-bit untouched). |
| Suspension state | Detects a Terms-of-Service suspension only from an explicit agy message, marks the account `TermsSuspended`, and shows it prominently (CLI and TUI banner) with a pointer to the Antigravity FAQ and Google support. | Retry it automatically — refresh, watch, the TUI and the monitor's `agy models` catalog tick all skip it unless you explicitly force a check (`--force-refresh`, or `agyva models --refresh`) — or auto-switch away from it (route automation ranks it ineligible). |
| `watch`, `monitor`, TUI | Observe; a switch from the TUI needs explicit confirmation and uses the same journaled path as `agyva use`. | Switch accounts on their own, warm up quota windows, or retry on quota errors. |
| TUI secrets and logging | Logging is off in TUI mode, and the terminal is restored on exit, panic and Ctrl+C. | Prompt for or display secrets. In passphrase mode it refuses to switch unless `AGYVA_PASSPHRASE` is set, because it will not read a passphrase while the screen is in raw mode. |
| Desktop notifications | Carry an alias and a state. | Carry credentials. |

The whole-archive check is part of the import contract: every entry — including
ones that will be skipped — is validated before any destination state is read,
and imports only ever add.

### Self-update

`src/updater.rs` is the one deliberate network surface.

- HTTPS GETs to the GitHub API releases endpoint and the release asset URLs,
  with `https_only` enforced, a hard timeout and size ceilings.
- Every request and every redirect hop is restricted to `api.github.com`,
  `github.com`, `objects.githubusercontent.com`, and
  `release-assets.githubusercontent.com`.
- The decompressed stream as a whole is bounded, not just each member.
- Archives are verified against `SHA256SUMS` and, when GitHub exposes one, the
  per-asset digest (a malformed digest is refused, never treated as absent).
- Every tar member is screened (no traversal, no links, plain directories
  tolerated, exactly one executable), and the binary is staged and renamed
  atomically.
- Only supported release targets (`x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`,
  `x86_64-pc-windows-msvc`, and `aarch64-pc-windows-msvc`) query or install
  releases; unsupported targets skip the startup check and refuse `agyva update`.
- The startup check is passive: one metadata request, silent failure, cached in
  `<cache>/update-check.json` (mode 0600) for 24 hours on success and 1 hour on
  failure. It runs at the start of every invocation whose config allows it —
  including JSON output, completions and the TUI, which makes it once per
  process and shows the result on the dashboard. What those modes suppress is
  only the notice on stderr, never the request.
- `check_updates = false` or `AGYVA_UPDATE_CHECK=0` disables the request itself,
  and the check never runs for the `update` command, which makes its own single
  metadata request.

### Opt-in automation

The one exception to "never switches", and it is fenced:

- A `routing.toml` group must set `auto_switch = true` **and** you must run
  `agyva config set auto_switch_ack true`; the acknowledgment is separate from
  `policy_acknowledged` and is re-settable to `false` to disable.
- The only driver is `agyva monitor --group NAME`'s evaluation cycle, so
  automation exists only while that foreground loop runs.
- It fires only after a **fresh** live quota reading — never on cached data,
  never on cached reset times.
- It never picks a `TermsSuspended` or otherwise ineligible account.
- It defers whenever agy sessions are detected (or detection is uncertain), or
  while a switch journal is pending recovery.
- It waits at least `auto_switch_cooldown_secs` (default 900 seconds; overridable
  per group via `cooldown_secs`) between any two switches; a manual `agyva use`
  also re-arms this cooldown. Standby candidates use cache readings up to
  `auto_switch_candidate_max_age_secs` (default 21600 seconds) and must clear
  `five_hour_threshold_percent` / `weekly_threshold_percent` and
  `hysteresis_percent` (defaults 10%).
- It re-validates everything under the login lock and then uses the same
  journaled switch path as `agyva use`.
- Every automatic decision — including holds and defers — is recorded in
  `agyva route history`, and the cooldown bookkeeping lives in
  `<state dir>/auto-switch.json` (mode 0600, schema 1).

The default remains off.

## Switch safety

A switch is journaled, checkpointed and verified rather than assumed:

- A switch never overwrites an agy login agyva has not saved unless you pass
  `--force`; the unsaved login is then kept in a **pinned** encrypted
  checkpoint (`<state>/checkpoints/*.keep.ckpt`) that is never pruned by
  ordinary retention, and the command prints its path. Use
  `agyva checkpoints list|restore|prune` to inspect, restore, or prune them.
- agy's login is re-read immediately before the write; if it changed, the
  switch aborts with nothing written.
- `recover` reads agy's current login first. If it matches neither the
  checkpointed login nor the target, it refuses unless `--force`, which first
  keeps a pinned, encrypted copy of the current login.
- A keyring write or delete that times out has an **unknown outcome**. Agyva
  does not roll back (that could overwrite a login that did change); the
  journal stays `RecoveryRequired` (exit 3) until you resolve any unlock prompt
  and run `agyva recover`.
- A failure after the registry commit (journal clear, checkpoint pruning) is a
  warning, not an error.

## Known limits

- **Checkpoints retain old credentials** (encrypted, newest 5). After you
  revoke a login at Google, an older checkpoint may still hold the revoked
  token until it is pruned. Revoked tokens are inert, but remove
  `<state>/checkpoints` if you want them gone sooner.
- Anyone who can unlock your keyring session, or who knows the passphrase, can
  read the vault. Agyva does not defend against malware running as you.
- Health text is matched against agy's wording. If agy changes its messages,
  health degrades to `unknown` rather than guessing; a suspension message that
  is worded differently would not be recognised.
- The TUI's refresh thread is not joined at quit; an in-flight `agy /usage`
  child is bounded by its 15 s timeout.
- **Rollback of old encrypted blobs is not detected.** Envelopes bind their
  context, so a file cannot be moved between accounts, but an attacker who can
  write to your config directory could restore an older, valid copy of a
  snapshot (an older token). Revoked tokens are inert; this is a documented
  limitation, not a defended property.
- **Residual race window.** Between agyva's last read of agy's login and its
  write, another process could still change it. The re-read narrows the window
  to the gap between that read and the write call; it cannot be closed without
  cooperation from agy.
- **Encryption does not defend against a compromised running session.**
  Software running as you with an unlocked keyring can read the vault key and
  agy's own live login directly.
- Pinned checkpoints hold old logins until you restore or prune them
  (`agyva checkpoints prune --include-pinned`) or delete them from
  `<state>/checkpoints`.
- JWT claims are read without signature verification; they are labels only.
- A running agy may refresh its token after a switch; agyva saves the outgoing
  account's live credential back on the next switch, not continuously.

## Unverified compatibility

The [compatibility matrix](compatibility.md) separates enforced rules, locally
observed agy 1.5.9 shapes, reference assumptions and unverified behavior.

Agyva writes the keyring item `gemini`/`antigravity` with label
`Password for 'antigravity' on 'gemini'` and `xdg:schema =
org.freedesktop.Secret.Generic`, using the keyring-core Secret Service store.
Whether agy (go-keyring) accepts an item written this way, including content
type (`application/octet-stream` here versus go-keyring's `text/plain`), is
**not verified against a real agy**. The opt-in real-keyring test
(`tests/real_keyring.rs`) exercises only a throwaway item and was NOT RUN. Use the file backend
(`backend = file`) if your agy reads the token file.

## Compatibility policy

Agyva accepts agy 1.5.x by default. Consumer credential and quota shapes were
observed on 1.5.9; native keyring writes remain unverified. Other versions are
refused unless you pass `--allow-untested` or set `allow_untested_agy = true`.
A login whose shape is not recognized is refused rather than guessed at.

## Reporting

Report vulnerabilities privately to the repository owner; do not file public
issues containing credentials.
