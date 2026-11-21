# Compatibility

[README](../README.md) · [Commands](commands.md) · [Security](security.md) · [Antigravity setup](antigravity.md)

Evidence labels used below:

- **ENFORCED:** Rule checked by `agyva` itself.
- **OBSERVED-LOCALLY:** Observed once on a real `agy 1.5.9` (Linux); test fixtures derive from that run.
- **FROM-REFERENCE:** Derived from public `agy` documentation or design constraints; exercised with synthetic data.
- **UNVERIFIED:** Implemented without real-`agy` or target-hardware evidence.

## Matrix

| Area | Label | Notes |
| --- | --- | --- |
| `agy --version` gate | ENFORCED (gate); OBSERVED-LOCALLY (format, 1.5.9) | Parses `agy X.Y.Z`, accepts `1.5.x`, and refuses others or undetectable versions unless `--allow-untested` or `allow_untested_agy = true`. |
| `agy -p "/usage" --output-format json` | OBSERVED-LOCALLY (1.5.9) | Parses `{status, response, usage}` and `command.data.groups[].buckets[]` (`window` `5h`/`weekly`, `remaining_fraction`, `reset_time`). Unknown shapes map to `Unavailable(schema_changed)`. |
| Active-only quota | FROM-REFERENCE (design constraint) | Assumes `/usage` inspects the active login only; `agyva` never swaps credentials to query standby accounts. |
| Credential document shape | OBSERVED-LOCALLY (1.5.9, consumer, Linux) | Observed keys: `token` (`access_token`, `token_type`, `refresh_token`, `expiry`, `expires_in`), `auth_method` (`"consumer"`), `id_token_email`. `id_token` is optional. GCP (`project_id`/`region`) shapes are accepted but **UNVERIFIED**. Identity precedence: `id_token` `sub`, JWT `email`, `id_token_email`, `sha256(refresh_token)`. Written verbatim. |
| Keyring item location | FROM-REFERENCE | `service=gemini`, `username=antigravity`. |
| Token file location | FROM-REFERENCE | `~/.gemini/antigravity-cli/antigravity-oauth-token` (use `backend = file`). |
| Keyring write byte-compatibility | **UNVERIFIED** | `agyva` writes `application/octet-stream` with label `Password for 'antigravity' on 'gemini'` and `xdg:schema = org.freedesktop.Secret.Generic`; `go-keyring` writes `text/plain`. Whether real `agy` accepts overwritten keyring items is unverified. |
| Companion state & custom provider sync | FROM-REFERENCE | Account switches touch `installation_id` and `settings.json` `gcp` block. Endpoint activation (`use ep:<name>`) writes `modelProvider` and `customModelsConfig` (no API keys) and clears them on OAuth switch. |
| Running-session detection | FROM-REFERENCE | Non-blocking `flock` probe on `presence/*.lock` plus current-UID `agy` (`agy.exe` on Windows) process scan. Advisory only. |
| ToS suspension text | FROM-REFERENCE | Matched against known wording; unrecognized wording degrades to `unknown`. |
| macOS Keychain & Windows Credential Manager | **UNVERIFIED** (hardware) | Implemented (`apple-native-keyring-store`, `windows-native-keyring-store`), CI-tested with sandboxed/mock stores (`macos-latest`, `windows-latest`), unverified on local hardware. |

## Platform support

- **Linux (`x86_64`, `aarch64`, glibc):** Verified locally with Secret Service (GNOME Keyring, KeePassXC, KWallet) and `backend = file`.
- **macOS (`x86_64`, `aarch64`) & Windows (`x86_64`, `aarch64` MSVC):** Keyring backends and release builds are implemented and CI-configured, unverified locally on hardware.
- **Headless / no keyring:** `vault_key = auto` falls back to a passphrase vault; `agy` must use `backend = file`.

Restart `agy` after any switch.

## Keyring timeouts

`agyva` bounds keyring calls so a locked keyring cannot hang forever:
- **Read timeout:** Returns an ordinary error (exit 1).
- **Write/delete timeout:** Returns `OutcomeUnknown` (exit 3) and leaves the journal in `RecoveryRequired` without rolling back. Unlock the keyring, then run `agyva recover`.

## Checking it yourself

Test with a disposable login and keep a backup of your token file or keyring item first:

1. Run `agyva version` and `agyva doctor`.
2. Run `agyva add`, `agyva use <other>`, then start `agy` and confirm the active login changed.
3. Optional real Secret Service round-trip test (uses throwaway item `agyva-test-<random>`, never `gemini`/`antigravity`/`agyva`; **NOT RUN** for release):

   ```sh
   AGYVA_REAL_KEYRING_TEST=1 cargo test --test real_keyring -- --ignored
   ```

If step 2 fails on keyring writes, use `backend = file` if supported and report `agy --version` plus `agyva doctor` output.
