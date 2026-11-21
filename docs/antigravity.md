# Antigravity setup and quota

[README](../README.md) · [Commands](commands.md) · [Compatibility matrix](compatibility.md)

Agyva manages existing Antigravity CLI (`agy`) logins. Install `agy` separately
and sign in using its normal flow before saving each account with `agyva add`.
It moves the stored credential as opaque bytes; it does not patch the binary,
create an OAuth client or call a private Google endpoint.

## Choose a credential store

Linux (Secret Service or file) is the locally verified target. macOS (Apple
Keychain) and Windows (Windows Credential Manager) keyring backends are
implemented and tested in CI, though unverified locally on macOS or Windows
hardware. The default `backend = auto` discovers agy’s credential store, using
the platform OS keyring (freedesktop Secret Service over D-Bus on Linux/BSD,
Apple Keychain on macOS, Windows Credential Manager on Windows) or the native
token file. Common Linux Secret Service providers include GNOME Keyring,
KeePassXC and KWallet with its Secret Service bridge. Agyva’s byte compatibility
when writing agy’s keyring item remains **unverified against real agy**; read
the [compatibility matrix](compatibility.md#matrix).

For agy installations that keep their login in
`~/.gemini/antigravity-cli/antigravity-oauth-token`:

```sh
agyva config set backend file
agyva doctor
```

On a headless or no-D-Bus system, Agyva’s vault can use a passphrase
(`vault_key = passphrase`, or automatically with `auto` when no keyring answers).
Agy’s own login must still be in a store Agyva can reach. If it exists only in
an unreachable keyring, switching cannot work. The file backend only helps
when your agy reads that token file.

The version gate accepts 1.5.x. Consumer credential and quota shapes were
observed on `agy 1.5.9`; enterprise/GCP credentials and native keyring writes
have narrower evidence. `agyva version` and `doctor` explain the detected
setup. `--allow-untested` or `allow_untested_agy = true` bypasses the gate,
without making the unknown version compatible.

## Read quota and health

```sh
agyva usage --all
agyva usage --all --no-refresh
agyva health --no-refresh
```

The [official usage guide](https://antigravity.google/docs/cli/commands/usage/)
describes quota groups and weekly/five-hour windows. Agyva obtains its data
only from `agy -p "/usage" --output-format json`. The exact JSON envelope,
including `command.data.groups[].buckets[]`, is a **locally observed,
undocumented schema**. Official documentation of the command does not make
that envelope a public API contract. Unrecognized responses become
unavailable; Agyva never fills in a missing number.

Only the active account can refresh. Other accounts show their last cached
reading, or no data, with an explanation. Switch manually and restart agy to
refresh another account. Missing five-hour or weekly windows say `not reported`;
this does not imply a full or exhausted allowance. Disabled groups are not
entitled, distinct from exhausted enabled groups. Plans are not reported.

Normal refreshes are at least 60 seconds apart (`quota_min_refresh_secs`).
Readings are fresh for 300 seconds (`quota_fresh_secs`), then stale, and expired
after 24 hours or the configured freshness interval if longer. Failed or
throttled refreshes retain the prior numbers and observation timestamp.
The dashboard continues aging freshness while it is open; a cache observation
never verifies a live login.

The documentation’s [sanitized real-cache example](capture-data/accounts.json)
has two accounts. Personal Gemini weekly remaining is `98.9199995994568%`;
work is `100%`. Both have an exhausted Claude and GPT group with `0%` weekly
remaining. No five-hour window or plan was reported. Both health readings
are cached `quota_limited`, with observation times on November 15, 2026.
The [Usage capture](tui-usage.svg) displays these group values separately.

`healthy` requires quota returned in the current run; health read from cache
is not shown as healthy. A Terms of Service suspension requires an explicit
message from agy; a bare HTTP 403 or `PERMISSION_DENIED` is ambiguous and maps
to `unknown`. Suspended, revoked, expired and login-required states survive
transient failures. Normal refresh, `watch` and the dashboard never retry a
suspended account. `--force-refresh` requests one explicit check.

## Policy and restarts

Google’s [Antigravity FAQ](https://antigravity.google/docs/faq/) says third-party
tools and account rotation are not supported on consumer plans. Agyva is
unofficial and manual. You are responsible for complying with Google’s terms;
it cannot prevent suspension or work around one. The first `add` or `use`
prints a policy notice.

After a switch, **restart agy**. Running sessions keep the login they started
with. Agyva only reports detected sessions; it never signals or kills them.
The [security guide](security.md) explains encryption and the limits of local
switch verification.
