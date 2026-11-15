# Security and policy

## Policy

Agyva is a **manual** account switcher. It switches only when you run
`agyva use` or confirm a dashboard switch. It does not rotate, balance or auto-switch accounts, does not
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

- Saved credentials and checkpoints are AES-256-GCM envelopes: versioned
  header, random 96-bit nonce, and AAD binding the format version, algorithm
  and a context string (`snapshot:<id>`, `checkpoint:<txn>`). A file moved to
  another account's slot, a flipped bit or a changed version fails to open.
- The vault key lives in the OS keyring (`agyva`/`vault-key`). If no keyring
  is reachable, or `vault_key = passphrase`, the key is derived with Argon2id
  (19 MiB, t=2, p=1 by default) from a passphrase of at least 8 characters
  taken from a TTY prompt or `AGYVA_PASSPHRASE`; never from argv. The mode is
  fixed at vault creation. KDF parameters read back from `vault.json` are
  bounds-checked (memory 8 KiB to 1 GiB, t and p 1 to 16, salt 8 to 64 bytes);
  anything outside is treated as a corrupt vault, so a tampered file cannot
  demand absurd memory or time.
- No plaintext key file, no plaintext credential copies, no `.bak` files.
- Files are created 0600 and directories 0700; writes are atomic
  (temp file, fsync, rename, directory fsync).
- `AGYVA_PASSPHRASE` is moved out of the environment map into a zeroizing
  holder at startup and is never passed to child processes (`agy` runs with
  all `AGYVA_*` variables removed). It cannot be removed from the process's own
  environment without `unsafe` code, which the project does not use; prefer the
  interactive prompt.
- Account ids and checkpoint names read from disk (registry, journal) are
  validated (10 lowercase hex characters; a fixed name shape) before any file
  path is built from them.
- Test hooks are fenced: `AGYVA_BACKEND` (which swaps agy's credential store
  and seals the keyring) is honored only when the config, state and agy-home
  directories are redirected too; `AGYVA_FAILPOINT` only with
  `AGYVA_TEST_MODE=1`.
- Secrets are held in zeroizing wrappers with redacted `Debug`. Keyring errors
  are mapped by variant so their text cannot echo secret bytes. JSON output
  and error messages never include credential material.

## Usage, health and the TUI

- Quota comes only from agy's own `/usage` command for the active login.
  Agyva makes no network request of its own for quota, uses no OAuth client of
  its own and never passes a credential to agy; agy is run with no stdin and
  with all `AGYVA_*` variables (including the vault passphrase) removed from
  its environment. Its output is size-bounded and time-limited.
- The one deliberate network surface is self-update (`src/updater.rs`): HTTPS
  GETs to the GitHub API releases endpoint and the release asset URLs, with
  `https_only` enforced, a hard timeout and size ceilings. Every request and
  every redirect hop is restricted to `api.github.com`, `github.com` and
  `objects.githubusercontent.com`; the decompressed stream as a whole is
  bounded, not just each member. Archives are verified against `SHA256SUMS`
  and, when GitHub exposes one, the per-asset digest (a malformed digest is
  refused, never treated as absent); every tar member is screened (no
  traversal, no links, plain directories tolerated, exactly one executable),
  and the binary is staged and renamed atomically. The startup check is
  passive: one metadata request, silent failure. It runs at the start of every
  invocation whose config allows it — including JSON output, completions and
  the TUI, which makes it once per process and shows the result on the
  dashboard. What those modes suppress is only the notice on stderr, never the
  request. `check_updates = false` or `AGYVA_UPDATE_CHECK=0` disables the
  request itself, and the check never runs for the `update` command, which
  makes its own single metadata request.
- Export archives (`core::transfer`) are sealed with AES-256-GCM under the
  dedicated `archive:1` context with a key derived from a **separate** archive
  passphrase via the production Argon2id profile; the vault key is never
  exported. The whole archive — including entries that will be skipped — is
  validated before any destination state is read, and imports only ever add:
  existing identities are skipped bit-for-bit untouched.
- The quota cache (`<cache>/quota.json`, 0600) holds percentages, reset times,
  health state and a one-line rule description. It holds no tokens, no email
  and no raw agy output. The TUI and JSON output show the same data. Tests
  assert none of the synthetic secrets appear in the cache or output.
- A Terms-of-Service suspension is detected only from an explicit agy message.
  The account is then marked `TermsSuspended`, shown prominently (CLI and TUI
  banner) with a pointer to the Antigravity FAQ and Google support, and is
  **never retried automatically**: refresh, watch and the TUI skip it unless
  you pass `--force-refresh`. Agyva does not help work around a suspension, and
  it does not auto-switch to another account because of one.
- `watch` and the TUI observe; they never switch accounts on their own, never
  warm up quota windows and never retry on quota errors. A switch from the TUI
  needs explicit confirmation and uses the same journaled path as
  `agyva use`.
- The TUI never prompts for or displays secrets. In passphrase mode it refuses
  to switch unless `AGYVA_PASSPHRASE` is set (it will not read a passphrase
  while the screen is in raw mode). Logging is off in TUI mode, and the
  terminal is restored on exit, panic and Ctrl+C.
- Desktop notifications carry an alias and a state, never credentials.

## Switch safety

- A switch never overwrites an agy login agyva has not saved unless you pass
  `--force`; the unsaved login is then kept in a **pinned** encrypted
  checkpoint (`<state>/checkpoints/*.keep.ckpt`) that is never pruned, and the
  command prints its path. agyva cannot restore it yet; it exists so the login
  is not lost.
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
- Pinned checkpoints hold old logins until you delete them by hand.
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
