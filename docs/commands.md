# Commands

[README](../README.md) · [Dashboard](dashboard.md) · [JSON and exit codes](json.md) · [Security](security.md)

Run `agyva --help` for task groups and examples, or `agyva <command> --help`
for flags. Bare `agyva` opens the dashboard when stdin and stdout are terminals;
otherwise it prints help.

## Save and manage accounts

Save the login `agy` already holds, and manage the saved copies.

```sh
agyva add --alias personal            # save agy's current login
agyva add --alias personal --refresh  # update an already saved login
agyva list
agyva current
agyva rename personal home
agyva remove work                     # asks for confirmation
```

Sign in with `agy` before each `add`; Agyva does not sign in for you. `remove`
forgets the saved snapshot and leaves agy’s native login untouched. Removing
the active account needs `--force`; `-y` skips confirmation for scripts. JSON
shapes are in [JSON and exit codes](json.md).

## Switch and restart

Activate a saved account through a journaled, verified switch.

```sh
agyva use personal
agyva use 2
agyva use -                     # previous account
```

Selectors accept an alias, list number, exact email, account id, unique id
prefix, or `-` for the previous account. Ambiguous selectors fail with exit 2.
If agy holds an unsaved login, save it with `add` first; `use <selector> --force`
replaces it only after keeping a pinned encrypted checkpoint. There is currently
no command to restore a pinned checkpoint — see
[switch safety](security.md#switch-safety).

**Restart `agy` after switching.** Existing sessions keep their old login;
Agyva reports detected sessions and never kills them. A switch never happens
automatically. Exit code 3 and the recovery procedure are in
[Recover an interrupted switch](#recover-an-interrupted-switch).

## Inspect quota and health

Read cached evidence for one account or all of them; only the active account can
refresh.

```sh
agyva usage                     # active account; normal refresh
agyva usage --all               # other accounts use the cache
agyva usage work --no-refresh   # cached data only
agyva health --no-refresh       # health for every saved account
agyva watch                     # foreground observer, Ctrl+C stops
```

| Flag | Effect |
| --- | --- |
| `--all` | Include every saved account (`usage` only). |
| `--no-refresh` | Read the cache; never run agy. |
| `--force-refresh` | Ignore the minimum interval and check a suspended account once; conflicts with `--no-refresh`. |
| `--json` | Machine-readable output — see [JSON and exit codes](json.md). |

Normal refreshes respect the minimum interval and skip suspended accounts; only
the active account can refresh. `watch` observes the active account at the
configured interval, backs off on transient failures, and never switches or warms
quota windows. Optional desktop notifications contain an alias and state, not
credentials. [Windows, groups and freshness](antigravity.md#read-quota-and-health).
Model catalogs have their own command: [Model catalog](#model-catalog).

### Model catalog

Show the cached catalog behind `--for-model`: each account’s families, when agy
last reported them, and whether that observation is still fresh.

```sh
agyva models            # cache only; never runs agy
agyva models --refresh  # one bounded `agy models` run for the active account
agyva models --json
```

A catalog counts as fresh for `models_cache_secs` (default one day). Without
`--refresh` the command never runs agy; `--refresh` forces one bounded run (10 s
timeout, refresh lock, attribution checked before and after) and a failure keeps
the prior observation. Accounts other than the active one only get a catalog by
being active when it runs. `--refresh` runs under the same version gate as every
write path: an untested agy is skipped, and `--allow-untested` (or
`allow_untested_agy = true`) is the only bypass — `--refresh` does not override
the gate. A dedicated interactive TUI route tab is not in 0.1.1; the route pane
ships in the watch-style terminal monitor instead.

### Route-aware monitor

Print the sticky pick plus each account’s share, freshness and exclusion reason,
with the next weekly reset countdown.

```sh
agyva monitor [--interval 15s] [--group NAME] [--for-model FAMILY]
```

Every tick re-ranks the cached evidence (never an agy spawn). `--interval`
accepts `15s`, `30`, `2m`, `1h` with a 5 s floor — a faster loop changes nothing,
because the evaluation is cache-only. The active account’s quota refresh runs on
its own 300 s cadence through the existing single-flight refresh guard, so
several monitors and a manual `usage` collapse to one run; the same tick keeps
the active account’s model catalog current (bounded `agy models`, throttled by
`models_cache_secs`, skipped while the observation is fresh). `monitor` never
switches accounts unless the group explicitly opts in — see
[Opt-in auto-switch](#opt-in-auto-switch).

## Plan a route

Rank cached readings without running agy or changing any account, then optionally
activate the recommendation.

```sh
agyva route list                    # groups and their activation mode
agyva route inspect GROUP           # cached evidence per account
agyva route plan GROUP [--json]     # recommended account
agyva route plan                    # every saved account, no group
agyva route apply GROUP [--yes]     # one journaled switch
agyva route history [--limit N]     # recent decisions and reasons
```

### Groups

`route list` reads named groups from `routing.toml`, beside `config.toml`.

```toml
[groups.work]
accounts = ["work", "personal"] # selectors; stable account IDs are recommended
auto_switch = false             # opt-in automation
ranking = "smart"               # or "priority" / "least-used"
[groups.work.priorities]
work = 10                       # higher values first with priority ranking
[groups.work.models]
"gemini-4-carbon" = "Gemini Pro" # maps a model family to its quota group
[groups.work.plans]
work = "pro"                     # declared plan for least-used ranking
```

| Field | Meaning |
| --- | --- |
| `accounts` | Selectors for the group; listed order breaks ties, and an empty list includes every saved account. |
| `ranking` | `smart` (default), `priority` (declared number) or `least-used` (declared plan). |
| `priorities` | Per-account number for `priority` ranking; larger comes first. |
| `plans` | Per-account `free` / `pro` / `ultra`; agy never reports a plan, so this is the only source. |
| `models` | Model family → quota group, so `--for-model` knows which bucket pays. |
| `auto_switch` | Opt-in automation for this group; see [Opt-in auto-switch](#opt-in-auto-switch). |
| `five_hour_threshold_percent` | Active five-hour share at or below which auto-switch may trigger (0–100, default `10.0`). |
| `weekly_threshold_percent` | Active weekly share at or below which auto-switch may trigger (0–100, default `10.0`). |
| `hysteresis_percent` | Minimum margin a healthy candidate must lead the active account by (0–100, default `10.0`). |
| `cooldown_secs` | Optional per-group switch cooldown override in seconds (30–86400; defaults to `auto_switch_cooldown_secs`). |

`route list` names each group’s ranking and its real activation mode: `manual activation`, the opt-in still
missing its acknowledgement (it prints the `config set auto_switch_ack true` hint), or `auto-switch enabled
(acknowledged)`.

### Inspect and plan

`route inspect GROUP` and `route plan GROUP` rank cached readings without running
agy or changing any account; with no group, `route plan` considers all saved
accounts. `smart` ranks by tier, freshness and renewal; `priority` and
`least-used` demote only ineligible accounts and then apply their own order
(declared number, declared plan), exactly as the unfiltered plan does, so a cheap
or preferred account is never displaced by a rate cap; ties keep the group’s
listed order.

Smart ranks the fullest counting-window share, holds near-full five-hour rate
caps back, and spends weekly budgets renewing sooner first. Unknown, stale,
incomplete, reset-expired, unavailable and suspended accounts are excluded with
reasons, and `route plan` exits 0 whether or not one is eligible — check
`recommendation` in `--json` or the "No eligible account in the cache." line. A
reset passing never fabricates new quota. Only an eligible row shows a quota
percentage: an excluded row shows `-` in place of its share, so a stale reading
can never read as available quota (`--json` still carries the raw
`remaining_percent` next to `eligible: false` and the reason).

`least-used` orders by declared plan instead — free before pro before ultra, with
undeclared accounts last and smart’s ordering breaking ties — so a cheaper
subscription is spent first.

### --for-model

Keep only accounts whose observed model catalog offers the family.
`route inspect|plan --for-model <model>` filters on that catalog. The catalog
comes from a bounded `agy models` run for the active account only, cached for
`models_cache_secs`; [`agyva models --refresh`](#model-catalog) runs it on
demand, [`agyva monitor`](#route-aware-monitor)’s tick keeps it current, and a
failure keeps the prior observation. A missing or stale catalog never proves
support, and the built-in reference list is descriptive only.

`--for-model` accepts the family id or any effort-suffixed id the account reports
(`gemini-4-carbon-high`), case-insensitively; a variant resolves to its family’s
mapping, so both spellings give the same share. An id that is a standalone
catalog entry (nothing else shares its base) keeps its own identity instead of
being re-based.

The family must also be mapped: when no quota group reports it and
`routing.toml [groups.<name>.models]` declares nothing, the account is excluded
with a reason naming that fix — the whole-account share is not evidence about a
family no local group pays for. When a group does map to the family (declared
mapping first, then group names), the ranked share and eligibility follow that
group’s windows — an exhausted unrelated bucket cannot hide a usable one — and
rows are re-ranked on those numbers.

### Apply

`agyva route apply GROUP` activates the group’s recommended account through the
same journaled, checkpointed switch as `agyva use`: exactly one switch,
revalidated under the lock (recommendation, group file, cached evidence, pending
recovery) immediately before anything changes.

It asks for confirmation first (`--yes` to skip); if agy sessions are running it
warns that they keep their old login. If the evidence moved between planning and
switching, apply refuses and says so: `--force` overrides the drift check, never
the recovery check. Auto-switch passes its `--for-model` filter into apply, so
the account it activates is the one the pane and the decision named —
revalidation ranks the same family, never the whole account. `--json` requires
`--yes` because it never prompts on stdout.

### History

`agyva route history [--limit N]` shows the most recent decisions with their
reasons — aliases and outcomes only, never credentials. Automatic holds and
defers are recorded alongside applied switches, with consecutive identical
outcomes collapsed so a steady hold cannot flush the log.

### Opt-in auto-switch

Off by default, and doubly gated: a group in `routing.toml` must set
`auto_switch = true`, **and** you must run
`agyva config set auto_switch_ack true` once. With both set, the evaluation loop
of `agyva monitor --group NAME` (15 s tick, 300 s single-flight active refresh)
runs `route apply` for that group when its gates pass — the monitor is the
driver, so automation happens only while it runs, and the pane prints
`auto-switch applied: <alias>` when it acts; `route apply` itself never
auto-confirms, so automation can only happen through the monitor’s cycle.

It still refuses to act on stale evidence (the cycle must have won a live active
reading, never a cached reset time), never picks a suspended or otherwise
ineligible account, defers while agy sessions are detected (or detection is
uncertain) or a switch journal awaits recovery, and waits at least
`cooldown_secs` / `auto_switch_cooldown_secs` (default 900 seconds) between
switches — a manual `agyva use` re-arms that cooldown too. Standby candidates
use cache readings up to `auto_switch_candidate_max_age_secs` (default
21600 seconds) and must clear `five_hour_threshold_percent` /
`weekly_threshold_percent` and `hysteresis_percent` (defaults 10%). Holds and
defers are recorded in `agyva route history`, and the cooldown bookkeeping lives
in `<state dir>/auto-switch.json` (mode 0600, schema 1). Set `auto_switch_ack`
back to `false` (or `auto_switch` to `false`) to stop it.

## Recover an interrupted switch

Finish or undo a switch that did not complete.

```sh
agyva status     # state and any pending recovery
agyva doctor     # read-only checks
agyva recover    # complete a verified switch, or restore the checkpoint
```

Exit 3 means recovery is required. Resolve an outstanding keyring unlock prompt
before recovery. A timed-out write has an unknown outcome, so Agyva leaves the
journal for recovery rather than overwriting a possibly completed write.
`recover` completes a verified switch or otherwise restores the checkpoint;
`recover --rollback` explicitly chooses rollback. If agy’s live login matches
neither side of the interrupted switch, recovery refuses to overwrite it —
inspect `status` first, and note that `recover --force` keeps a pinned encrypted
copy of that current login before restoring. A failure after the registry commit
is a cleanup warning; the switch itself is complete.

| Symptom | Next step |
| --- | --- |
| Unsupported agy version | `agyva version`; consult [compatibility](compatibility.md) before using `--allow-untested`. |
| Live login changed during a switch | Another process wrote it; retry after that activity finishes. |
| Vault does not unlock | Check the passphrase or keyring item. A passphrase vault cannot be recovered without its passphrase. |
| Quota unavailable | Read the reason and source; run `doctor`. A cached reading does not verify the current login. |
| Passphrase switch refused in the dashboard | Use `agyva use` in a shell so the vault can prompt. |

## Move accounts between machines

Write and read portable, passphrase-encrypted archives.

```sh
agyva export /home/you/agyva-export.agy           # all saved accounts
agyva export /home/you/one.agy --account work     # a single account
agyva import /home/you/agyva-export.agy
```

`export` writes a passphrase-encrypted archive (always a fresh file; an existing
destination is never overwritten) and requires an absolute path. The archive
passphrase is separate from your vault passphrase and is asked for
interactively, or supply `AGYVA_ARCHIVE_PASSPHRASE` (minimum 8 characters).
Exports use a 64 MiB Argon2id profile (`t=3`, `p=1`) regardless of your local
vault settings, while `import` accepts archives down to the 19 MiB (`t=2`,
`p=1`) floor for compatibility with existing archives; the vault key is never
exported.

`import` adds accounts that are missing and skips ones already saved — an import
never modifies or removes anything in the destination. All entries are validated
before anything changes, so a bad archive leaves your accounts untouched.
Identities already saved keep their local aliases; conflicts (a new account
claiming an alias you already use, or an id collision with a different identity)
refuse the whole import with a hint. An interrupted import is completed or rolled
back automatically on the next command.

## Update Agyva

Check for a newer stable release, or install it.

```sh
agyva update                      # download, verify and install
agyva update --install            # same, spelled out
agyva update --check              # report only; installs nothing
```

Updates come from this project’s GitHub Releases on supported targets
(`x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`): the release
tarball and its `SHA256SUMS` are verified before the running binary is replaced
atomically. Every invocation also checks for a newer stable release once at
startup (2 s timeout, silent on failure, cached in `<cache>/update-check.json`
for 24 hours on success and 1 hour on failure, skipped on unsupported targets)
and prints a one-line notice on stderr; disable that with `check_updates = false`
in the config or `AGYVA_UPDATE_CHECK=0` in the environment.
[Self-update internals](architecture.md#self-update).

## Rekey the vault

Rotate the vault key or passphrase, or migrate between keyring and passphrase
storage.

```sh
agyva rekey                      # rotate key or passphrase in the current mode
agyva rekey --mode passphrase    # migrate to a passphrase-protected vault
agyva rekey --mode keyring       # migrate to the OS keyring
```

`rekey` unlocks the existing vault (using the keyring or `AGYVA_PASSPHRASE`),
decrypts all saved snapshots and checkpoints in memory, re-encrypts them under
the new key, and commits `vault.json` atomically. In passphrase mode, supply the
new passphrase interactively or via `AGYVA_NEW_PASSPHRASE` (minimum 8
characters). Pending switch recovery blocks `rekey` with exit 3.

## Settings

`agyva config show` lists values and their meanings. `agyva config path` prints
the file path; `agyva config set <key> <value>` validates before saving.

| Setting | Default / range |
| --- | --- |
| `allow_untested_agy` | `false`; accept agy versions outside the known-good range? |
| `auto_switch_ack` | `false`; explicit acknowledgement that opt-in route automation may switch accounts. |
| `auto_switch_candidate_max_age_secs` | `21600`; maximum cache age in seconds for standby auto-switch candidates, 60–604800. |
| `auto_switch_cooldown_secs` | `900`; minimum seconds between switches before auto-switch may run again, 30–86400. |
| `backend` | `auto`; `secret-service` or `file` chooses agy’s native store. |
| `check_updates` | `true`; check GitHub Releases for newer agyva at startup. |
| `checkpoints_keep` | `5`; ordinary checkpoints retained, 1–20. Pinned copies are never pruned. |
| `color` | `auto`; `always` or `never`. Nonempty `NO_COLOR` disables colors; pipes stay plain. |
| `desktop_notifications` | `false`; let `agyva watch` send desktop notifications. |
| `lock_timeout_secs` | `10`; seconds to wait for another agyva, 0–600. |
| `models_cache_secs` | `86400`; how long a saved model catalog counts as fresh, 60–604800. |
| `policy_acknowledged` | `false`; whether the one-time policy notice was shown. |
| `quota_fresh_secs` | `300`; how long a quota reading counts as fresh, 10–86400. |
| `quota_min_refresh_secs` | `60`; minimum seconds between quota refreshes, 10–3600. |
| `vault_key` | `auto`; `keyring` or `passphrase`. Sets initial vault mode; use `agyva rekey` to rotate or migrate an existing vault. |
| `watch_interval_secs` | `300`; seconds between `agyva watch` checks, 60–86400. |

State and cache use XDG directories. `AGYVA_LOG=debug` sends logs to stderr;
logging is disabled in the dashboard. Development overrides and isolated
backends are in [development](development.md#environment-overrides).

## Shell completion

Print a completion script for your shell.

```sh
mkdir -p ~/.config/fish/completions
agyva completions fish > ~/.config/fish/completions/agyva.fish

mkdir -p ~/.local/share/bash-completion/completions
agyva completions bash > ~/.local/share/bash-completion/completions/agyva

mkdir -p ~/.zfunc
agyva completions zsh > ~/.zfunc/_agyva
```

For Zsh, add `~/.zfunc` to `$fpath` in your shell configuration. Fish also
completes saved aliases by running `agyva list --json`, which reads only the
public registry and needs no keyring access.
