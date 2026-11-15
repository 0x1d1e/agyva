# Commands

[README](../README.md) · [Dashboard](dashboard.md) · [JSON and exit codes](json.md)

Run `agyva --help` for task groups and examples, or `agyva <command> --help`
for flags. Bare `agyva` opens the dashboard when stdin and stdout are terminals;
otherwise it prints help.

## Save and manage accounts

```sh
agyva add --alias personal       # save agy's current login
agyva add --alias personal --refresh  # update an already saved login
agyva list
agyva current
agyva rename personal home
agyva remove work                # asks for confirmation
```

Sign in with `agy` before each `add`; Agyva does not sign in for you. `remove`
forgets the saved snapshot and leaves agy’s native login untouched. Removing
the active account needs `--force`; `-y` skips confirmation for scripts.

## Switch and restart

```sh
agyva use personal
agyva use 2
agyva use -                     # previous account
```

Restart `agy` after switching. Existing sessions keep their old login; Agyva
reports detected sessions and never kills them. Switching uses a checkpoint,
journal and local read-back verification. It never happens automatically.

Selectors accept an alias, list number, exact email, account id, unique id
prefix, or `-` for the previous account. Ambiguous selectors fail with exit 2.
If agy holds an unsaved login, save it with `add` first. `use <selector> --force`
replaces it only after keeping a pinned encrypted checkpoint. There is currently
no command to restore a pinned checkpoint; see [security](security.md#switch-safety).

## Inspect quota and health

```sh
agyva usage                     # active account; normal refresh
agyva usage --all               # other accounts use the cache
agyva usage work --no-refresh   # cached data only
agyva health --no-refresh       # health for every saved account
agyva watch                     # foreground observer, Ctrl+C stops
agyva models                    # cached model catalog per account
agyva models --refresh          # run `agy models` for the active account
```

`usage` and `health` accept a selector, `--json`, `--no-refresh`, and
`--force-refresh`. `usage` also accepts `--all`. Normal refreshes respect the
minimum interval and skip suspended accounts; `--force-refresh` overrides both
for one explicit check. It conflicts with `--no-refresh`. Only the active
account can refresh. [Windows, groups and freshness](antigravity.md#read-quota-and-health).

`watch` observes the active account at the configured interval, backs off on
transient failures, and never switches or warms quota windows. Optional desktop
notifications contain an alias and state, not credentials.

`agyva models --refresh` runs `agy models` for the active account under the
same version gate as every write path: an untested agy is skipped, and
`--allow-untested` (or `allow_untested_agy = true`) is the only bypass:
`--refresh` does not override the gate.

`agyva monitor [--interval 15s] [--group NAME] [--for-model FAMILY]` is the
route-aware variant: every tick re-ranks the cached evidence (never an agy
spawn) and prints the sticky pick plus each account's share, freshness and
exclusion reason, with the next weekly reset countdown. `--interval` accepts
`15s`, `30`, `2m`, `1h` with a 5s floor (a faster loop changes nothing — the
evaluation is cache-only). The active account's quota refresh runs on its own
300s cadence through the existing single-flight refresh guard, so several
monitors and a manual `usage` collapse to one run; the same tick also keeps
the active account's model catalog current (bounded `agy models`, throttled
by `models_cache_secs`, skipped entirely while that observation is fresh).
`monitor` never switches accounts unless the group explicitly opts in — see
[Opt-in auto-switch](#opt-in-auto-switch).

`agyva models [--refresh] [--json]` shows the cached catalog behind
`--for-model`: each account's families, when agy last reported them and
whether that observation is still fresh. Without `--refresh` it never runs
agy. `--refresh` forces one bounded `agy models` run for the active account
only (10s timeout, refresh lock, attribution checked before and after), and
a failure keeps the prior observation. Accounts other than the active one
only get a catalog by being active when it runs. (A dedicated interactive TUI route tab is not in 0.1.1; the route
pane ships in this watch-style terminal monitor instead.)

## Plan a route

`agyva route list` lists named groups from `routing.toml` beside `config.toml`.
`agyva route inspect GROUP` and `agyva route plan GROUP --json` rank cached
readings without running agy or changing any account. With no group, `route plan`
considers all saved accounts. Activation is manual.

```toml
[groups.work]
accounts = ["work", "personal"] # selectors; stable account IDs are recommended
auto_switch = false # opt-in automation; see "Opt-in auto-switch" below
ranking = "smart" # or "priority" (listed order + priorities) / "least-used"
[groups.work.priorities]
work = 10 # higher values first with priority ranking
[groups.work.models]
"gemini-4-carbon" = "Gemini Pro" # maps a family to its quota group
[groups.work.plans]
work = "pro" # declared plan for least-used ranking (alias or id)
```

`route inspect|plan --for-model <model>` keeps only accounts whose observed model
catalog offers the family. The catalog comes from a bounded `agy models` run
for the active account only, cached for `models_cache_secs` (default one
day); `agyva models --refresh` runs it on demand, `agyva monitor`'s tick keeps
it current, and a failure keeps the prior observation. A missing or stale
catalog never proves support, and the built-in reference list is descriptive
only.

`--for-model` accepts the family id or any effort-suffixed id the account
reports (`gemini-4-carbon-high`), case-insensitively; a variant resolves to
its family's mapping, so both spellings give the same share. An id that is a
standalone catalog entry (nothing else shares its base) keeps its own
identity instead of being re-based.

The family must also be mapped: when no quota group reports it and
`routing.toml [groups.<name>.models]` declares nothing, the account is
excluded with a reason naming that fix — the whole-account share is not
evidence about a family no local group pays for. When a group does map to
the family (declared mapping first, then group names), the ranked share and
eligibility follow that group's windows — an exhausted unrelated bucket
cannot hide a usable one — and rows are re-ranked on those numbers.
`smart` ranks by tier, freshness and renewal; `priority` and `least-used`
demote only ineligible accounts and then apply their own order (declared
number, declared plan), exactly as the unfiltered plan does, so a cheap or
preferred account is never displaced by a rate cap; ties keep the group's
listed order.

`agyva route apply GROUP` activates the group's recommended account through
the same journaled, checkpointed switch as `agyva use` — exactly one switch,
revalidated under the lock (recommendation, group file, cached evidence,
pending recovery) immediately before anything changes. It asks for
confirmation first (`--yes` to skip); if agy sessions are running it warns
that they keep their old login. If the evidence moved between planning and
switching, apply refuses and says so (`--force` overrides the drift check,
never the recovery check). Auto-switch passes its `--for-model` filter into
apply, so the account it activates is the one the pane and the decision
named: revalidation ranks the same family, never the whole account.
`agyva route history [--limit N]` shows the most recent decisions with their
reasons — aliases and outcomes only, never credentials.

`least-used` orders by declared plan instead — free before pro before ultra,
with undeclared accounts last and smart's ordering breaking ties — so a
cheaper subscription is spent first. `route list` names each group's ranking
and its real activation mode (`manual activation`, an opt-in still missing
the policy acknowledgement, or `auto-switch enabled (acknowledged)`).

Smart ranks the fullest counting-window share, holds near-full five-hour rate
caps back, and spends weekly budgets renewing sooner first. Ties retain listed
order. Unknown, stale, incomplete, reset-expired, unavailable and suspended
accounts are excluded with reasons, and `route plan` exits 0 whether or not
one is eligible — check `recommendation` in `--json` or the
"No eligible account in the cache." line. A reset passing never fabricates
new quota. Only an eligible row shows a quota percentage: an excluded row
shows `-` in place of its share, so a stale reading can never read as
available quota (`--json` still carries the raw `remaining_percent` next to
`eligible: false` and the reason).

### Opt-in auto-switch

Off by default, and doubly gated: a group in `routing.toml` must set
`auto_switch = true`, **and** you must run
`agyva config set auto_switch_ack true` once. With both set, the evaluation
loop of `agyva monitor --group NAME` (15s tick, 300s single-flight active
refresh) runs `route apply` for that group when its gates pass — the monitor
is the driver, so automation happens only while it runs, and the pane prints
`auto-switch applied: <alias>` when it acts; `route apply` itself never
auto-confirms, so automation can only happen through the monitor's cycle.

It still refuses to act on stale evidence (the cycle must have won a live
active reading, never a cached reset time), never picks a suspended or
otherwise ineligible account, defers while agy sessions are detected (or
detection is uncertain) or a switch journal awaits recovery, and waits at
least 900 seconds between switches — a manual `agyva use` re-arms that
cooldown too. Holds and defers are recorded in `agyva route history`
alongside applied switches, with consecutive identical outcomes collapsed so
a steady hold cannot flush the log. Set `auto_switch_ack` back to `false`
(or `auto_switch` to `false`) to stop it.

## Recover an interrupted switch

Start with the read-only commands:

```sh
agyva status
agyva doctor
agyva recover
```

Exit 3 means recovery is required. Resolve an outstanding keyring unlock prompt
before recovery. A timed-out write has an unknown outcome, so Agyva leaves the
journal for recovery rather than overwriting a possibly completed write.
`recover` completes a verified switch or otherwise restores the checkpoint;
`recover --rollback` explicitly chooses rollback.

If agy’s live login matches neither side of the interrupted switch, recovery
refuses to overwrite it. Inspect `status` first. `recover --force` keeps a pinned
encrypted copy of that current login before restoring. A failure after the
registry commit is a cleanup warning; the switch itself is complete.

| Symptom | Next step |
| --- | --- |
| Unsupported agy version | `agyva version`; consult [compatibility](compatibility.md) before using `--allow-untested`. |
| Live login changed during a switch | Another process wrote it; retry after that activity finishes. |
| Vault does not unlock | Check the passphrase or keyring item. A passphrase vault cannot be recovered without its passphrase. |
| Quota unavailable | Read the reason and source; run `doctor`. A cached reading does not verify the current login. |
| Passphrase switch refused in the dashboard | Use `agyva use` in a shell so the vault can prompt. |

## Move accounts between machines

```sh
agyva export /home/you/agyva-export.agy           # all saved accounts
agyva export /home/you/one.agy --account work     # a single account
agyva import /home/you/agyva-export.agy
```

`export` writes a passphrase-encrypted archive (always a fresh file; an
existing destination is never overwritten). The archive passphrase is separate
from your vault passphrase and is asked for interactively, or supply
`AGYVA_ARCHIVE_PASSPHRASE` (minimum 8 characters). It uses the production
Argon2id profile regardless of your local vault settings.

`import` adds accounts that are missing and skips ones already saved — an
import never modifies or removes anything in the destination. All entries are
validated before anything changes, so a bad archive leaves your accounts
untouched. Identities already saved keep their local aliases; conflicts (a new
account claiming an alias you already use, or an id collision with a different
identity) refuse the whole import with a hint. An interrupted import is
completed or rolled back automatically on the next command.

## Update Agyva

```sh
agyva update --check              # report only
agyva update                      # download, verify and install
```

Updates come from this project's GitHub Releases: the release tarball and its
`SHA256SUMS` are verified before the running binary is replaced atomically.
Every invocation also checks for a newer stable release once at startup (2 s
timeout, silent on failure) and prints a one-line notice on stderr; disable
that with `check_updates = false` in the config or `AGYVA_UPDATE_CHECK=0` in
the environment.


## Settings

`agyva config show` lists values and their meanings. `agyva config path` prints
the file path; `agyva config set <key> <value>` validates before saving.

| Setting | Default / range |
| --- | --- |
| `backend` | `auto`; `secret-service` or `file` chooses agy’s native store. |
| `vault_key` | `auto`; `keyring` or `passphrase`. The mode is fixed when the vault is created. |
| `color` | `auto`; `always` or `never`. Nonempty `NO_COLOR` disables colors; pipes stay plain. |
| `allow_untested_agy` | `false`; prefer the supported version gate. |
| `checkpoints_keep` | `5`; ordinary checkpoints retained, 1–20. Pinned copies are never pruned. |
| `lock_timeout_secs` | `10`; 0–600. |
| `policy_acknowledged` | `false`; records the first-use policy notice. |
| `auto_switch_ack` | `false`; explicit acknowledgment that opt-in route automation may switch accounts: `true` or `false`. |
| `quota_fresh_secs` | `300`; 10–86400. |
| `quota_min_refresh_secs` | `60`; 10–3600. |
| `watch_interval_secs` | `300`; 60–86400. |
| `desktop_notifications` | `false`. |
| `check_updates` | `true`; set `false` to skip the startup update check. |

State and cache use XDG directories. `AGYVA_LOG=debug` sends logs to stderr;
logging is disabled in the dashboard. Development overrides and isolated
backends are in [development](development.md#environment-overrides).

## Shell completion

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
