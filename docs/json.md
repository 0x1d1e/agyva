# JSON output and scripting

[README](../README.md) · [Commands](commands.md) · [Quota semantics](antigravity.md#read-quota-and-health) · [Security](security.md)

Every `--json` command emits one JSON object on stdout with `schema_version: 1`, no ANSI codes, and no secrets. Warnings and hints go to stderr; errors keep their normal exit codes. Bare `agyva` prints help when stdout/stdin are not TTYs.

```sh
agyva list --json | jq '.accounts[] | {number, alias, active}'
agyva usage --all --no-refresh --json | jq '.accounts[] | {alias, freshness, observed_at, groups}'
```

Use `id` as the stable key (`number` is list position only). `email` may be `null`.

## `list` and `current`

`list --json` reads only `registry.json` (no keyring access):

| Field | Meaning |
| --- | --- |
| `active` | Registry active account id (or `null`). |
| `accounts[].number` | 1-based list position. |
| `accounts[].id` | 10-hex stable account id. |
| `accounts[].alias`, `email`, `auth_method` | Account metadata (`email` may be `null`). |
| `accounts[].active`, `previous` | Registry pointers (not live login check). |
| `accounts[].added_at`, `last_used_at` | UTC timestamps (`last_used_at` may be `null`). |

`current --json` returns `logged_in`, `managed`, `id`, `email`, `auth_method`, and `account` (`null` if unsaved or logged out).

## `usage` and `health`

| Field | Meaning |
| --- | --- |
| `notice` | Top-level reason when no refresh ran (`null` otherwise). |
| `accounts[].freshness` | `fresh` / `stale` / `expired` / `missing`. |
| `accounts[].observed_at`, `last_successful_update`, `last_attempt` | UTC timestamps or `null`. |
| `accounts[].refreshed_now` | `true` if `agy` returned data in this run. |
| `accounts[].groups[]` | `group`, `model`, `disabled`, `exhausted`, `five_hour`, `weekly`. |
| `groups[].five_hour` / `weekly` | `remaining_percent`, `reset_at`, `observed_at`, `source` (`cache` \| `agy_usage_command`), optional `burn_rate` (`percent_per_hour`, `eta_exhaustion_secs`, `exhausts_before_reset`, `samples`). |
| `accounts[].plan` | `null` when unreported (`agy /usage` does not report plans). |
| `accounts[].unavailable` | Reason when refresh failed/skipped (may accompany retained cache). |
| `accounts[].health` | `state`, `label`, `evidence`, `observed_at`, `source`, `verified_now`, `warning`. |

Unreported windows and resets are `null`. See [sanitized full output](capture-data/accounts.json).

## `models`

- `refresh`: `null` unless `--refresh` was passed (then `account_id`, `status`: `refreshed`\|`skipped`\|`failed`, optional `note`).
- `accounts[]`: `observed_at`, `age_secs`, `fresh`, `state` (`fresh`\|`stale`\|`missing`), `families[]` (`id`, `display_name`, `variants`).

## `route`

- `route plan` / `inspect --json`: `schema_version`, `generated_at`, `recommendation` (id or `null`), `accounts[]` (`id`, `eligible`, `freshness`, `remaining_percent`, `rate_remaining_percent`, `renewals`, `why`, `command`). Note: `accounts[].remaining_percent` carries the raw cached percentage even when `eligible: false`.
- `route apply --json` (requires `--yes`): `group`, `target`, `target_alias`, `changed`, `already_active`, `reason`, `declared_plan`, `warnings`.
- `route history --json [--limit N]`: `schema_version`, `entries[]` (`at`, `target`, `outcome`: `applied`\|`refused`\|`failed`, `group`, `reason`), newest first, clamped 1-50.
- `route init|add|remove --json`: updated `RoutingFile` (`remove --json` requires `--yes`).

## `endpoint`, `profile`, and `use`

| Command | Top-level JSON fields |
| --- | --- |
| `use --json` | `UnifiedUseOutcome`: `kind: "account"` (`account`, `already_active`, `warnings`, `policy_notice`) or `kind: "endpoint"` (`endpoint`). With `--profile`: `profile`, `switched_account`, `active_endpoint`, `warnings`. |
| `endpoint presets --json` | `schema_version`, `presets[]` (`name`, `display_name`, `base_url`, `protocol`, `auth_scheme`, `default_model`, `api_version`). |
| `endpoint list --json` | `schema_version`, `endpoints[]` (`id`, `alias`, `base_url`, `protocol`, `detected_protocol`, `default_model`, `active`, `enabled`, `model_count`, `models[]`, `fetched_at`, `latency_ms`, `rate_limits`, `last_error`). |
| `endpoint show --json` | Metadata, `effective_protocol`, `auth_scheme`, `extra_headers`, `active`, `enabled`, timestamps, `models[]`, `rate_limits`, `last_error`. |
| `endpoint test --json` | `reachable`, `detected_protocol`, `latency_ms`, `model_count`, `models[]`, `rate_limits`, `error`. |
| `endpoint models --json` | `schema_version`, `endpoint`, `id`, `refreshed`, `model_count`, `models[]`, `fetched_at`, `latency_ms`, `rate_limits`. |
| `endpoint add\|update\|rename\|remove\|use --json` | `add`: `endpoint`, `activated`, `probe`; `use`: `cleared`, `endpoint`; `update`/`rename`/`remove`: `endpoint` (`remove --json` requires `--yes`). |
| `profile ... --json` | `list`: `profiles[]`; `show`/`which`: `profile`; `init`/`create`: `path`, `name`, `source`, `profile`; `set`: `name`, `field`, `profile`; `delete`: `deleted` (`--yes` required); `use`: `ProfileUseOutcome`. |

## `statusline`, `watch`, and `monitor`

- `statusline --format json`: `schema_version`, `active_account`, `active_account_id`, `active_endpoint`, `active_endpoint_model`, `active_profile`, `lowest_five_hour_percent`, `lowest_weekly_percent`, `burn_rate_percent_per_hour`, `eta_exhaustion_secs`, `health`, `suspended`, `recovery_pending`, `threshold_alert`.
- `statusline --format waybar`: `{"text", "tooltip", "class"}` (`ok` \| `warning` \| `critical` \| `suspended`).
- `watch --json-stream`: NDJSON lines with `at` and flattened `WatchEvent` (`started`, `cycle_ok`, `cycle_failed`, `note`, `health_changed`, `reset_approaching`, `suspended_skipped`).
- `monitor --json` (implies `--once`): `MonitorSnapshot` (`schema_version`, `generated_at`, `group`, `for_model`, `active_endpoint`, `active_profile`, `recommendation`, `sticky_pick`, `last_auto`, `rows[]`). `monitor --json-stream` emits NDJSON with `at`, `WatchEvent`, and `snapshot` on `cycle_ok`.

## `checkpoints`, `status`, and `doctor`

- `status --json`: `agyva_version`, `agy`, `agy_path`, `quota_provider`, `accounts`, `active`, `backend`, `vault`, `config_dir`, `state_dir`, `sessions` (`presence_locks`, `agy_pids`, optional `stale_presence_locks`, optional `uncertain`), `recovery_pending` (`null` or `{txn_id, target, phase, started_at, detail}`).
- `doctor --json`: `schema_version`, `healthy`, `recovery_pending`, `checks[]` (`name`, `level`, `detail`, `hint`). `doctor --fix --json` adds `actions[]` and nested `report`. Both exit 3 when `recovery_pending` is true.
- `checkpoints list --json`: `schema_version`, `checkpoints[]` (`name`, `txn_id`, `created_at`, `pinned`, `size_bytes`, `valid`). `restore`/`prune --json` require `--yes`.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Error (or `doctor` found a failing check). |
| `2` | Usage error: bad arguments, unknown/ambiguous selector, invalid setting. |
| `3` | Recovery required: pending switch journal or `OutcomeUnknown` keyring timeout. Run `agyva recover`. |
| `99` | Simulated crash (`AGYVA_TEST_MODE=1` failpoint only). |

`color = never` or nonempty `NO_COLOR` disables ANSI styling; JSON is always plain.
