# Compatibility

[README](../README.md) · [Commands](commands.md) · [Security](security.md) · [Antigravity setup](antigravity.md)

What agyva assumes about `agy`, and how much of it has actually been checked.
Three labels are used throughout:

- **ENFORCED**: a rule agyva itself applies (for example, refusing any version
  outside 1.5.x). No claim is made about agy beyond what agyva checks.
- **OBSERVED-LOCALLY**: a real `agy 1.5.9` was run once during development and
  produced this output; the test fixture is derived from that run. No raw
  transcript is committed, so treat it as one observation, not a guarantee.
- **FROM-REFERENCE**: taken from public descriptions of `agy`'s behavior, or a
  design constraint of agyva, and exercised only with synthetic data. Treat
  as likely, not proven.
- **UNVERIFIED**: implemented on a best guess; no real-`agy` evidence.

## Matrix

| Area | Label | Notes |
| --- | --- | --- |
| `agy --version` prints `agy X.Y.Z`; version gate | ENFORCED (gate); OBSERVED-LOCALLY (format, 1.5.9) | Agyva parses `agy X.Y.Z`, supports 1.5.x, and refuses anything else unless `--allow-untested` or `allow_untested_agy = true`; an undetectable version is also refused. That the output has this form was seen once on 1.5.9. |
| `agy -p "/usage" --output-format json` | OBSERVED-LOCALLY (1.5.9) | Envelope `{status, response, usage}` and `command.data.groups[].buckets[]` with `window` (`5h` or `weekly`), `remaining_fraction` (0..1), `reset_time` (UTC). Seen once on 1.5.9; the fixture is derived from that run and no raw transcript is committed. Undocumented, so any other shape becomes `Unavailable(schema_changed)`. Groups may be `disabled`, shown as "not entitled", not "exhausted". |
| Quota only for the logged-in account | FROM-REFERENCE (design constraint) | Agyva's design assumes `/usage` answers for the active login, so it never switches or injects credentials to read another account. This was not verified as an agy behavior on its own. |
| Credential document shape | OBSERVED-LOCALLY (agy 1.5.9, consumer, Linux) | Live consumer item had exactly three top-level keys: `token` (`access_token`, `token_type`, `refresh_token`, `expiry` as RFC3339 with 9 fractional digits and a numeric `±hh:mm` offset, `expires_in`), `auth_method` (`"consumer"`) and `id_token_email` (a plain email, not a JWT). `id_token` is optional and omitted on consumer logins; agy's binary accepts either `id_token` or `id_token_email`. Agyva also accepts GCP-shaped documents with `project_id`/`region`, but that shape is UNVERIFIED. Handled as opaque bytes and written back verbatim. Identity precedence: decodable `id_token` `sub`, then JWT `email`, then the plain `id_token_email`, then a `sha256(refresh_token)` fingerprint; a malformed `id_token` falls back rather than refusing the credential. Residual uncertainty: one live sample; enterprise/GCP logins are unknown. |
| Native store location: Secret Service `service=gemini`, `username=antigravity` | FROM-REFERENCE | |
| Native store alternative: file `~/.gemini/antigravity-cli/antigravity-oauth-token` | FROM-REFERENCE | Select with `backend = file`. |
| **Byte-compatibility of agyva's keyring write with real agy** (attributes, label `Password for 'antigravity' on 'gemini'`, `xdg:schema`, content type) | **UNVERIFIED** | Agyva sets `application/octet-stream`; go-keyring (which agy is believed to use) writes `text/plain`. Reads of agy's own item are expected to work; whether agy accepts an item agyva created or overwrote is not proven. See "Checking it yourself". |
| Companion state: `installation_id`, `settings.json` `gcp` block, custom provider sync | FROM-REFERENCE | OAuth switches touch only the `gcp` block; custom endpoint activation (`use ep:<name>` / `endpoint use`) writes `modelProvider` and `customModelsConfig` (no API keys) and clears them when switching back to an OAuth account. |
| Running-session detection (`presence/*.lock`, process name `agy` / `agy.exe`) | FROM-REFERENCE | Advisory only; agyva never signals a process. Uses a non-blocking `flock` liveness probe on `presence/*.lock` (ignoring stale locks) and filters `agy` (`agy.exe` on Windows) processes to the current user. |
| Health wording (Terms of Service message) | FROM-REFERENCE | Matched against the wording seen in the reference material; a changed message degrades to `unknown`. |
| macOS Keychain, Windows Credential Manager | **UNVERIFIED** (hardware) | Implemented via `apple-native-keyring-store` and `windows-native-keyring-store`; built and tested with sandboxed/mock stores in CI (`macos-latest`, `windows-latest`) and packaged in `release.yml`. Real Keychain/Credential Manager hardware behavior is unverified on this Linux host. |

## Platform support

Linux (x86-64 and ARM64, glibc and musl) with the freedesktop Secret Service
(GNOME Keyring, KeePassXC, KWallet's bridge) is the locally verified target.
macOS (Apple Keychain, `x86_64` and `aarch64`) and Windows (Credential Manager,
`x86_64` MSVC; `aarch64` MSVC in updater) are implemented and CI-configured,
though unverified locally on macOS/Windows hardware. The file backend works
wherever `agy` keeps its token in the file. Without an OS keyring provider
(headless server, container, SSH without a session bus), agyva's own vault uses
a passphrase automatically (`vault_key = auto`) or by choice
(`vault_key = passphrase`); `agy`'s login still has to live somewhere agyva can
reach, which means `backend = file`.

After any switch, restart `agy`: a running session keeps the login it started
with.

## Keyring timeouts

A locked keyring may block on an unlock prompt. Agyva bounds the wait. A
timeout on a **read** is an ordinary error. A timeout on a **write or delete**
means the outcome is unknown (the item may or may not have changed), so agyva
does not roll back: the journal stays in `RecoveryRequired`, the command exits
3, and the hint is to resolve any keyring unlock prompt and run
`agyva recover`.

## Checking it yourself

Do these only with a login you can afford to lose, and keep a copy of the
token file or keyring item first.

1. `agyva version` and `agyva doctor`: version gate, login shape, store.
2. `agyva add`, then `agyva use <other>`, then start `agy` and confirm it is
   logged in as `<other>` (the real test of byte-compatibility).
3. The ignored opt-in test touches the real Secret Service daemon but only a
   throwaway item named `agyva-test-<random>` (never `gemini`, `antigravity`
   or `agyva`):

   ```sh
   AGYVA_REAL_KEYRING_TEST=1 cargo test --test real_keyring -- --ignored
   ```

   It was **NOT RUN** for this release. It verifies that the keyring layer
   round-trips an item and reports a locked keyring as documented, not that
   `agy` accepts agyva's item.

If step 2 fails on your `agy`, switch with `backend = file` (if your `agy`
supports the token file) and please report the `agy` version and the
`agyva doctor` output.
