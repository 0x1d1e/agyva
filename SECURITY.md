# Security policy

Report vulnerabilities privately before public disclosure. Never open a public issue containing credentials, tokens, emails, vault files, or keyring dumps.

## Supported versions

Only `main` and the latest `0.1.x` release are supported. There are no LTS branches.

## Reporting a vulnerability

Use GitHub's private advisory form: <https://github.com/0x1d1e/agyva/security/advisories/new>. If unavailable, contact the maintainer via GitHub.

Include:
- `agyva version` output and build source (release binary or source commit id).
- `agy` version and platform (OS, desktop/keyring backend, or `backend = file`).
- Expected vs actual behavior and minimal reproduction steps.
- Whether reproduction needs a real keyring or works in `scripts/demo-sandbox.sh`.

**Redact all secrets.** Replace tokens and emails with placeholders; never attach real `vault.json`, `.snap`, or `.ckpt` files. There is no bug bounty; reports are handled best-effort with coordinated disclosure.

## In scope

- **Vault and crypto:** AES-256-GCM envelopes, nonces, AAD binding, Argon2id key derivation, OS keyring / passphrase handling, file permissions, atomic writes.
- **Credential and secret isolation:** `agy` credential handling, identity derivation, endpoint API keys (`endpoints/*.ep.snap`), and preventing secret leaks in stdout, stderr, logs, `--json`, TUI, notifications, caches, or child environments.
- **Switch and recovery:** Transaction journal, checkpoints (`*.keep.ckpt`), rollback/roll-forward, concurrent modification detection, and `OutcomeUnknown` keyring timeouts.
- **Validation and gating:** Path/id/checkpoint name validation, selectors, sandbox gating (`AGYVA_BACKEND`), failpoint gating (`AGYVA_TEST_MODE`), endpoint URL checks, and bridge loopback binding.
- **Dependencies:** Reachable vulnerabilities in crates in `Cargo.lock`.

## Out of scope

- Google's Terms of Service, account policy, or suspensions.
- `agy`'s own behavior, undocumented `/usage` schema, or breaking changes in newer `agy` versions (file those as compatibility issues).
- Local attackers who can already run code as your user, unlock your keyring, or read your home directory.
- Rollback of an older valid encrypted snapshot by an attacker with write access to your config/state directory (documented in [`docs/security.md`](docs/security.md#known-limits)).
- The short residual race between `agyva`'s final read and write of `agy`'s login.
- Hardware-specific macOS Keychain or Windows Credential Manager issues that cannot be reproduced locally (both backends are implemented and CI-configured, unverified locally on hardware).
- Issues requiring `tests/real_keyring.rs` against real credential items or leaked personal data.

## Safe harbor

Good-faith research against your own test accounts and `scripts/demo-sandbox.sh` is welcome. Do not access other users' accounts or machines, run DoS traffic, or publish another person's credentials.

## Non-goals

`agyva` will not add background account rotation, quota-exhaustion retries, a private Google API client, User-Agent spoofing, or limit-evasion features.
