# JSON output and scripting

[README](../README.md) · [Commands](commands.md) · [Quota semantics](antigravity.md#read-quota-and-health) · [Security](security.md)

Use `--json` for machine-readable inspection. Successful output is one JSON
value on stdout, with `schema_version: 1`, no ANSI styling and no credentials.
Diagnostics, warnings and error hints belong on stderr. Errors keep their normal
exit codes rather than becoming a success-shaped JSON object.

```sh
agyva list --json
agyva current --json
agyva usage --all --no-refresh --json
agyva health --no-refresh --json
agyva models --json
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

## `agyva list` and `agyva current`

List the registry, or ask which login `agy` holds right now.

```json
{"schema_version":1,"active":"1a2b3c4d5e",
 "accounts":[{"number":1,"id":"1a2b3c4d5e","alias":"personal", …}]}
```

| Field | Meaning |
| --- | --- |
| `active` | Account id in the registry’s `active` slot. |
| `accounts[].number` | Current list position; not an identity. |
| `accounts[].id` | Stable 10-hex account id. |
| `accounts[].alias`, `email`, `auth_method` | Labels; `email` may be `null`. |
| `accounts[].active`, `previous` | Registry placement, not live verification. |
| `accounts[].added_at`, `last_used_at` | Timestamps; `last_used_at` may be `null`. |

`current --json` adds `logged_in`, `managed`, `id`, `email`, `auth_method` and a
nested `account` (the same shape, `null` when agy holds no saved login).

## `agyva usage` and `agyva health`

Read quota per group and window from the cache, with the active account possibly
refreshed in the same run.

```json
{"schema_version":1,"generated_at":"…","notice":null,
 "accounts":[{"alias":"work","freshness":"stale","groups":[…],"health":{…}}]}
```

| Field | Meaning |
| --- | --- |
| `notice` | Why nothing could be refreshed, when no account is involved. |
| `freshness` | `fresh` / `stale` / `expired` / `missing`. |
| `observed_at`, `last_successful_update`, `last_attempt` | Per-account timestamps, each possibly `null`. |
| `refreshed_now` | This run got data from agy. |
| `groups[]` | `group`, optional `model`, `disabled`, `exhausted`, optional `five_hour` and `weekly`. |
| `groups[].five_hour` / `weekly` | `remaining_percent`, `reset_at`, `observed_at`, `source` (`cache` or `agy_usage_command`). |
| `plan` | `null` when not reported — agy’s `/usage` does not report plans. |
| `unavailable` | Why there is no (new) reading; may accompany retained quota data. |
| `health` | `state`, `label`, `evidence`, `observed_at`, `source`, `verified_now`, `warning`. |

A missing window is `null`, not zero, and not full. Reset timestamps may also be
`null`. `refreshed_now` says that run got data from agy; `health.verified_now`
says agy answered during that run. Neither boolean changes a historical
timestamp into an ongoing verification. `unavailable` can accompany retained
quota data after a failed or skipped refresh; inspect its reason rather than
discarding the cache. The [full sanitized cache-only
output](capture-data/accounts.json) preserves exact per-group percentages and
timestamps from the installed application.

## `agyva models`

Report each account’s cached model catalog and, with `--refresh`, what the
attempt did.

```json
{"schema_version":1,"generated_at":"…","refresh":null,
 "accounts":[{"id":"…","state":"fresh","families":[{"id":"gemini-4-carbon", …}]}]}
```

| Field | Meaning |
| --- | --- |
| `refresh` | `null` unless `--refresh` was passed. |
| `refresh.account_id`, `status`, `note` | Target, `refreshed`/`skipped`/`failed`, and an optional note. |
| `accounts[].observed_at` | When agy last reported this catalog, or `null`. |
| `accounts[].age_secs`, `fresh` | Age and freshness under `models_cache_secs`. |
| `accounts[].state` | `fresh` / `stale` / `missing`. |
| `accounts[].families[]` | `id`, `display_name`, `variants`. |

Only a fresh catalog proves `--for-model` support; when it is missing, every
account is excluded and the `why` names `agyva models` as the fix.

## `agyva route plan`, `inspect` and `history`

Rank cached evidence without changing anything.

```json
{"schema_version":1,"generated_at":"…","recommendation":"1a2b3c4d5e",
 "accounts":[{"id":"…","eligible":true,"remaining_percent":72.5, …}]}
```

| Field | Meaning |
| --- | --- |
| `recommendation` | An account id or `null`. |
| `freshness` | `fresh` / `stale` / `expired` / `missing`. |
| `eligible` | Whether the account may be recommended or switched to. |
| `remaining_percent` | Raw cached share, even when `eligible` is false. |
| `rate_remaining_percent` | Five-hour rate-cap share. |
| `renewals` | The two reset timestamps, each possibly `null`. |
| `why` | Human reason; the machine-readable eligibility signal. |
| `command` | The command that would activate this row. |

Rows are ranked best-first with ineligible accounts last — including under
`--for-model`, where the order is recomputed from the mapped group’s windows and
then the group’s own ranking (`smart` by tier; `priority` and `least-used`
demoting only ineligible rows before applying their declared order; ties by the
listed order), so a row’s `remaining_percent` always reads consistently with its
position. The human tables print `-` instead of a share for ineligible rows, so
only an eligible row reads as available quota.

`route history --json [--limit N]` returns `{"schema_version", "entries"}` with
`at`, `target`, `outcome` (`applied`, `refused`, `failed`), an optional `group`
and a bounded `reason` per entry (automatic decisions included), newest first and
clamped to `--limit` (1–50, default 10) exactly like the human table.

`route apply GROUP --json` requires `--yes` — it never prompts on stdout — and
emits one object: `group`, `target`, `target_alias`, `changed`,
`already_active`, `reason`, `declared_plan` and `warnings`.

## `agyva status` and `agyva doctor`

Summarize local state and pending recovery, or run read-only checks.

| Field | Meaning |
| --- | --- |
| `agyva_version`, `agy`, `agy_path` | Versions and the detected executable. |
| `quota_provider` | Whether quota can be read, and why not. |
| `accounts`, `active` | Count, and the active account alias or `null`. |
| `backend`, `vault` | Native store and vault-key mode. |
| `config_dir`, `state_dir` | Resolved directories. |
| `sessions` | Detected agy sessions (`presence_locks`, `agy_pids`, optional `stale_presence_locks` omitted when zero, optional `uncertain`). |
| `recovery_pending` | `txn_id`, `target`, `phase`, `started_at`, optional `detail`; `null` when none. |

`doctor --json` reports `healthy` and `recovery_pending` booleans plus `checks[]`,
each with `name`, `level` and `detail`, plus an optional `hint`. Both commands
return exit 3 when recovery is pending, even though they print a full report.

This JSON schema is Agyva’s output contract. It is distinct from the
[undocumented agy usage envelope](compatibility.md#matrix) that Agyva parses.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Error; also `doctor` when it finds a failing check. |
| `2` | Usage error: invalid arguments, selector or setting. |
| `3` | Recovery required: interrupted switch or unknown keyring write outcome. Run `agyva recover` after resolving any unlock prompt. |

Check the exit status as well as parsing stdout. Exit 99 is reserved for the
test-mode crash simulation.

Configuration `color = never` and nonempty `NO_COLOR` disable terminal colors.
An empty `NO_COLOR` has no effect. JSON is plain regardless of color settings.
For refresh/switch safety and script confirmation flags, see [commands](commands.md).
