# Antigravity setup and quota

[README](../README.md) · [Commands](commands.md) · [Compatibility](compatibility.md)

`agyva` manages logins `agy` already holds. Install `agy` separately and sign in via its normal flow before running `agyva add`. `agyva` copies stored credentials as opaque bytes; it does not patch `agy`, create an OAuth client, or call private Google endpoints.

## Choose a credential store

Linux (Secret Service or file) is verified locally. macOS (Apple Keychain) and Windows (Credential Manager) backends are implemented and CI-configured, unverified locally on hardware.

Default `backend = auto` detects `agy`'s store: OS keyring (Linux Secret Service over D-Bus, macOS Keychain, Windows Credential Manager) or the native token file. Common Linux Secret Service providers are GNOME Keyring, KeePassXC, and KWallet. **Keyring write compatibility with real `agy` remains unverified**; see [compatibility](compatibility.md#matrix).

If your `agy` stores credentials in `~/.gemini/antigravity-cli/antigravity-oauth-token`:

```sh
agyva config set backend file
agyva doctor
```

On headless systems without D-Bus, `agyva`'s vault falls back to a passphrase (`vault_key = auto` or `passphrase`), while `agy`'s own login must use `backend = file`.

The version gate accepts `agy` `1.5.x` (consumer credential and quota shapes observed on `1.5.9`; enterprise/GCP shapes are unverified). Run `agyva version` and `agyva doctor` to inspect your setup. Use `--allow-untested` or `agyva config set allow_untested_agy true` to bypass the version gate at your own risk.

## Read quota and health

```sh
agyva usage --all
agyva usage --all --no-refresh
agyva health --no-refresh
```

Quota comes only from `agy -p "/usage" --output-format json` ([official usage guide](https://antigravity.google/docs/cli/commands/usage/)). The JSON envelope (`command.data.groups[].buckets[]`) is a locally observed, undocumented schema. Unrecognized responses become unavailable; missing values are never guessed.

- **Active account only:** Only the active account refreshes live. Other accounts show cached readings (or no data). Switch and restart `agy` to refresh another account.
- **Missing windows and disabled groups:** Unreported five-hour or weekly windows show `not reported`. Disabled groups show as not entitled rather than exhausted. Plans are not reported by `agy`.
- **Freshness and history:** Refreshes are throttled to `quota_min_refresh_secs` (60s). Readings are fresh for `quota_fresh_secs` (300s), stale afterward, and expired after 24h (or `quota_fresh_secs` if longer). Failed or throttled refreshes keep prior cache data. Observations accumulate in `<cache>/quota-history.json` (up to 64 samples per series) to compute burn rate (`%/h`) and exhaustion ETA.
- **Health and suspension:** `healthy` requires quota returned in the current run; cached health is never labeled `healthy`. Terms-of-Service suspension (`TermsSuspended`) requires an explicit message from `agy` (a bare HTTP 403 or `PERMISSION_DENIED` maps to `unknown`). Sticky states survive transient errors. Normal refresh, `watch`, `monitor`, and the TUI skip suspended accounts; `--force-refresh` checks once.

## Policy and restarts

Google's [Antigravity FAQ](https://antigravity.google/docs/faq/) states third-party tools and account rotation are unsupported on consumer plans. `agyva` is unofficial and manual by default; it cannot prevent or bypass suspensions. The first `add` or `use` prints this notice once.

**Restart `agy` after every switch.** Running `agy` sessions keep the login they started with. `agyva` reports detected sessions and never kills them. See [security](security.md) for encryption and verification limits.
