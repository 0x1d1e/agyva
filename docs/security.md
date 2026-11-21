# Security and policy

[README](../README.md) · [Commands](commands.md) · [Compatibility](compatibility.md) · [JSON and exit codes](json.md)

`agyva` moves `agy` logins between encrypted local slots and manages custom endpoint keys locally.

## Policy

`agyva` is a **manual** account switcher by default. It switches only on `agyva use` (or TUI confirmation), unless you explicitly opt a routing group into auto-switch. It never retries on quota errors or bypasses limits. Google's [Antigravity FAQ](https://antigravity.google/docs/faq/) states third-party tools and account rotation are unsupported on consumer plans; the first `add` or `use` prints this notice (`policy_acknowledged` in `config.toml`).

`agyva` does not run its own OAuth client, call private Google endpoints, or spoof User-Agents. After switching, **restart `agy`**; running sessions keep their startup login and `agyva` never kills processes.

## What is protected

- **AES-256-GCM envelopes:** Account snapshots (`<config>/accounts/<id>.snap`), endpoint secrets (`<config>/endpoints/<id>.ep.snap`), and checkpoints (`<state>/checkpoints/*.ckpt`) use versioned AES-256-GCM envelopes with random 96-bit nonces and AAD bound to context (`snapshot:<id>`, `endpoint:<id>`, `checkpoint:<txn>`).
- **Vault key and KDF:** Stored in the OS keyring (`agyva`/`vault-key`) or derived via Argon2id (default 19 MiB, `t=2`, `p=1`) from a passphrase (>= 8 chars via TTY prompt or `AGYVA_PASSPHRASE`, never argv). `agyva rekey` rotates keys or migrates modes atomically. KDF parameters read from `vault.json` are bounds-checked (8 KiB-1 GiB, `t`/`p` 1-16, salt 8-64 bytes).
- **No plaintext residue & atomic IO:** Directories are 0700, sensitive files 0600, writes use temp file + fsync + atomic rename + dir fsync, with no `.bak` files.
- **Passphrase hygiene:** `AGYVA_PASSPHRASE`, `AGYVA_NEW_PASSPHRASE`, and `AGYVA_ARCHIVE_PASSPHRASE` are moved into zeroizing holders at startup and stripped (`AGYVA_*`) from child `agy` processes. (Removing them from `agyva`'s own process env requires `unsafe`, which is forbidden crate-wide; prefer TTY prompts.)
- **Disk input validation:** Account ids (10 lowercase hex chars) and checkpoint filenames are validated before constructing paths.
- **Fenced test hooks:** `AGYVA_BACKEND` requires `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR`, and `AGYVA_AGY_HOME`. `AGYVA_FAILPOINT` requires `AGYVA_TEST_MODE=1`.
- **Output redaction:** Secrets use zeroizing wrappers with redacted `Debug` and never appear in JSON, error messages, logs, or notifications.

## Surface boundaries

| Surface | Behavior | Guarantees |
| --- | --- | --- |
| Quota runner | Spawns `agy -p "/usage" --output-format json` (no stdin, `AGYVA_*` stripped, 15s timeout). | No network calls or OAuth clients of its own. |
| Quota cache | `<cache>/quota.json` (0600) stores percentages, resets, health, rule text. | No tokens, emails, or raw `agy` stdout/stderr. |
| Custom endpoints & bridge | Encrypts keys in `endpoints/<id>.ep.snap`; allows `https://` or loopback/local `http://`; rejects `user:pass@` and `169.254/16`; binds `bridge` to loopback only; redacts secrets from errors. | Never writes API keys to `endpoints.json`, `settings.json`, `endpoint-models.json`, or `--json`. |
| Profiles | `.agyva.toml` and `profiles.toml` store non-secret defaults. | Refuses `env` keys containing `API_KEY`, `SECRET`, `TOKEN`, `PASSWORD`, `PASSPHRASE`. |
| Self-update | HTTPS GETs restricted to GitHub release hosts; verifies `SHA256SUMS` and asset digest. | Refuses malformed digests, empty/oversized binaries, or non-GitHub redirects. |
| Export archives | AES-256-GCM (`archive:1`) with separate passphrase via 64 MiB Argon2id (`import` accepts >= 19 MiB). | Never exports vault key; pre-validates archive; skips existing identities untouched. |
| Suspension state | `TermsSuspended` set only on explicit ToS message from `agy`. | Never retried by normal refresh, `watch`, TUI, `monitor`, or auto-switch. |
| `watch`, `monitor`, TUI | Read-only observers; TUI requires confirmation for switches and disables logging. | Passphrase TUI switches refuse without `AGYVA_PASSPHRASE` (no raw-mode prompt). |
| Notifications | Desktop alerts carry alias and state only. | Never carry credentials. |

### Self-update

`src/updater.rs` contacts only `api.github.com`, `github.com`, `objects.githubusercontent.com`, and `release-assets.githubusercontent.com` (`https_only`, bounded size/timeouts) on the 6 supported targets (`x86_64`/`aarch64` Linux `gnu`, macOS, Windows `msvc`). Startup checks cache results in `<cache>/update-check.json` (0600, 24h OK / 1h error) and are disabled by `check_updates = false` or `AGYVA_UPDATE_CHECK=0`.

### Opt-in automation

Auto-switch is off by default and gated by:
1. `auto_switch = true` in `routing.toml` **and** `auto_switch_ack = true` in `config.toml`.
2. Running `agyva monitor --group NAME` (only driver).
3. Fresh live active reading (never cached quota or elapsed resets), non-suspended eligible candidate within `auto_switch_candidate_max_age_secs` clearing threshold + hysteresis (default 10%).
4. No detected/uncertain `agy` sessions, no pending recovery, and elapsed cooldown (`cooldown_secs` / `auto_switch_cooldown_secs`, default 900s, persisted in `<state>/auto-switch.json`).
5. Lock-held revalidation immediately before journaled switch; all decisions logged to `route history`.

## Switch safety

- Unsaved live logins are never overwritten without `--force`, which saves a pinned `*.keep.ckpt` first.
- `agyva` re-reads `agy`'s live login right before writing; concurrent external changes abort with `ConcurrentChange` before any write.
- Keyring write/delete timeouts yield `OutcomeUnknown` (exit 3) without rollback; unlock the keyring and run `agyva recover`.
- `recover` refuses to overwrite an unrelated third login unless `--force` is passed (which keeps a pinned backup first).

## Known limits

- **Checkpoints retain old tokens** (encrypted, newest 5, plus pinned `*.keep.ckpt`) until pruned (`agyva checkpoints prune --include-pinned`) or deleted from `<state>/checkpoints`.
- **Unlocked session threat:** Any process running as your user with an unlocked keyring (or the passphrase) can read the vault and `agy`'s live login.
- **No encrypted-blob rollback detection:** An attacker with write access to `<config>` could restore an older valid `.snap` file (revoked tokens are inert).
- **Residual race window:** A tiny gap remains between `agyva`'s final pre-write read and the write itself.
- **Unverified JWT claims:** Account ids use JWT `sub` without signature verification (local label only).
- **Text-matched health:** Changed `agy` error wording degrades to `unknown`.
- **TUI background thread:** Not joined on exit; any running `agy /usage` child finishes within its 15s timeout.
- **Outgoing token refresh:** If `agy` refreshes its OAuth token mid-session, `agyva` captures it back on the next switch.

## Unverified compatibility

See [compatibility matrix](compatibility.md#matrix):
- Linux Secret Service write compatibility (`application/octet-stream` vs `go-keyring`'s `text/plain`) is unverified against real `agy`. Use `backend = file` if needed.
- macOS Keychain and Windows Credential Manager backends are implemented and CI-configured, unverified locally on hardware.
- Version gate accepts `agy` `1.5.x` (`1.5.9` observed); other versions require `--allow-untested` or `allow_untested_agy = true`.

## Reporting

Report vulnerabilities privately per [`SECURITY.md`](../SECURITY.md).
