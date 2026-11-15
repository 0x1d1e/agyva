# Security policy

Agyva handles account credentials, so security reports are taken seriously.
Please report privately first; do not open a public issue with credentials,
tokens, email addresses, vault files or keyring dumps in it.

## Supported versions

Only the latest commit on `main` and the most recent release are supported. The
project is at `0.1.x`; there are no long-term support branches.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for this repository:

<https://github.com/0x1d1e/agyva/security/advisories/new>

If that is unavailable, contact the maintainer through their GitHub profile
rather than posting details publicly. Include:

- the agyva version (`agyva version`) and how it was built (release tarball or
  from source, commit id);
- the `agy` version and platform (distribution, desktop environment;
  Secret Service provider or `backend = file`);
- what you expected, what happened, and the smallest reproduction you can
  describe;
- whether the issue needs a real keyring, an unlocked session or a local
  attacker, and whether you have already tested the same steps in a synthetic
  sandbox (`scripts/demo-sandbox.sh`).

**Redact secrets.** Replace tokens, refresh tokens, full email addresses and
keyring payloads with obvious placeholders. Do not attach a real vault,
`vault.json`, snapshot or checkpoint file. A synthetic reproduction is much
more useful than a real credential.

There is no bug bounty. Reports are handled on a best-effort basis; you will
get an acknowledgement and an assessment, and coordinated disclosure is
preferred. Please give a reasonable window to fix the issue before publishing.

## In scope

- The vault: AES-256-GCM envelope handling, nonce generation, authenticated
  data, key derivation (Argon2id), key storage in the OS keyring or a
  passphrase, file permissions and atomic writes.
- Credential handling: reading, moving and writing `agy`'s login verbatim,
  account identity derivation, deduplication, and anything that could leak a
  token into output, logs, `--json`, the TUI, a notification, a cache file or a
  child process environment.
- Switch and recovery: the journal, checkpoints, pinned `*.keep.ckpt` copies,
  rollback versus roll-forward, concurrent modification of `agy`'s login, and
  the unknown-outcome path after a keyring timeout.
- Path and input validation: account ids or checkpoint names read from disk,
  selectors, aliases, sandbox gating, failpoint gating.
- The TUI and `watch` as they relate to secret exposure or unsafe terminal
  handling.
- A dependency with a known vulnerability that is reachable from this code.

## Out of scope

- Google's terms, account policy or account suspension. Agyva is an unofficial
  manual switcher; see the policy section of the README. It does not rotate
  accounts, bypass limits or hide suspensions.
- The behavior of `agy` itself, its undocumented output schema, or changes in
  a new `agy` version. Report those as compatibility issues (they are not
  secrets), not as agyva vulnerabilities.
- An attacker who can already run code as you, unlock your keyring or read
  your home directory while the session is unlocked. Such an attacker can read
  `agy`'s live login, not only agyva's vault.
- Rollback of an old, valid encrypted snapshot by someone who can write to the
  config or state directory; this is a documented limitation in
  [`docs/security.md`](docs/security.md#known-limits), not a defended property.
- The short residual race between agyva's final read of `agy`'s login and its
  write, which cannot be closed without cooperation from `agy`.
- macOS and Windows support: it is not implemented.
- Issues that require the ignored real-keyring test to be run against real
  items, or that depend on data committed by mistake.

## Safe harbor

Good-faith research on your own accounts and a synthetic sandbox is welcome.
Do not access other people's accounts, data or machines, do not run denial of
service against any service, and do not publish another person's credentials.
If in doubt, ask privately before testing.

## Non-goals

Agyva will not add automatic account rotation, retry on quota exhaustion, a
private API client, User-Agent spoofing, or anything whose purpose is to evade
Google's limits or a suspension. Pull requests or reports that depend on such
behavior are outside this policy.
