# JSON output and scripting

[README](../README.md) · [Commands](commands.md) · [Quota semantics](antigravity.md#read-quota-and-health)

Use `--json` for machine-readable inspection. Successful output is one JSON
value on stdout, with `schema_version: 1`, no ANSI styling and no credentials.
Diagnostics, warnings and error hints belong on stderr. Errors keep their
normal exit codes rather than becoming a success-shaped JSON object.

```sh
agyva list --json
agyva current --json
agyva usage --all --no-refresh --json
agyva health --no-refresh --json
agyva status --json
agyva doctor --json
```

`list --json` reads only the public registry and needs no keyring access.
`usage --no-refresh` reads the quota cache, while ordinary `usage` may run agy
for the active account. Bare `agyva` prints help when its streams are not
terminals. Human output in a pipe is plain; use JSON rather than parsing a table.

## Select and inspect data

```sh
agyva list --json | jq '.accounts[] | {number, alias, active}'
agyva usage --all --no-refresh --json |
  jq '.accounts[] | {alias, freshness, observed_at, groups}'
```

Account ids are stable identifiers; list numbers are current positions, so do
not persist a number as an identity. Emails may be `null`. `active` records the
account selected in the registry, not a live identity check.

Each quota account includes its own `freshness`, `observed_at`,
`last_successful_update`, `last_attempt`, `refreshed_now`, `groups`,
`unavailable` and `health`. `plan` is `null` when not reported. Each group keeps
`group`, optional `model`, `disabled`, `exhausted`, and optional `five_hour`
and `weekly` windows. A present window has `remaining_percent`, `reset_at`,
`observed_at` and `source` (`cache` or `agy_usage_command`). A missing window is
`null`, not zero, and not full. Reset timestamps may also be `null`.

`agyva route plan [--for MODEL] [--json]` (named groups via
`route plan GROUP` / `route inspect GROUP`) returns
`{"schema_version", "generated_at", "recommendation", "accounts"}`. The
recommendation is an account id or `null`. Each account row carries `id`,
`alias`, `active`, `freshness` (`fresh`/`stale`/`expired`/`missing`),
`eligible`, `remaining_percent`, `rate_remaining_percent`, `renewals` (the
two reset timestamps, each possibly `null`), `why` (a human reason, the
machine-readable eligibility signal) and `command`. Rows are ranked
best-first with ineligible accounts last. `remaining_percent` is the raw
cached value even when `eligible` is false; the human tables print `-`
instead, so only an eligible row's share reads as available quota. `agyva
route history --json`
returns `{"schema_version", "entries"}` with `at`, `target`, `outcome`
(`applied`, `refused`, `failed`), an optional `group` and a bounded `reason`
per entry (automatic decisions included).

`refreshed_now` says that run got data from agy. `health.verified_now` says agy
answered during that run. Neither boolean changes a historical timestamp into
an ongoing verification. `unavailable` can accompany retained quota data after
a failed/skipped refresh; inspect its reason rather than discarding the cache.
The [full sanitized cache-only output](capture-data/accounts.json) preserves
exact per-group percentages and timestamps from the installed application.

This JSON schema is Agyva’s output contract. It is distinct from the
[undocumented agy usage envelope](compatibility.md#matrix) that Agyva parses.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Error; also `doctor` when it finds a failing check. |
| `2` | Usage error: invalid arguments, selector or setting. |
| `3` | Recovery required: interrupted switch or unknown keyring write outcome. Run `agyva recover` after resolving any unlock prompt. |

`status` and `doctor` can print their JSON report and still return exit 3 when
recovery is pending. Check the exit status as well as parsing stdout. Exit 99
is reserved for the test-mode crash simulation.

Configuration `color = never` and nonempty `NO_COLOR` disable terminal colors.
An empty `NO_COLOR` has no effect. JSON is plain regardless of color settings.
For refresh/switch safety and script confirmation flags, see [commands](commands.md).
