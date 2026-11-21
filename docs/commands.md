# Commands

[README](../README.md) · [Dashboard](dashboard.md) · [JSON and exit codes](json.md) · [Security](security.md)

Run `agyva --help` for task groups and examples, or `agyva <command> --help`
for flags. Global flags: `--allow-untested` (bypass the `agy` version gate) and
`-q` / `--quiet` (suppress non-essential notices). Bare `agyva` opens the
dashboard when stdin and stdout are terminals; otherwise it prints help.

## Save and manage accounts

Save the login `agy` already holds, and manage the saved copies.

```sh
agyva add --alias personal            # save agy's current login
agyva add --alias personal --refresh  # update an already saved login
agyva list                            # alias: ls
agyva current
agyva rename personal home
agyva remove work                     # asks for confirmation
```

Sign in with `agy` before each `add`; Agyva does not sign in for you. `remove`
forgets the saved snapshot and leaves agy’s native login untouched. Removing
the active account needs `--force`; `-y` skips confirmation for scripts. JSON
shapes are in [JSON and exit codes](json.md).

## Switch and restart

Activate a saved account, custom endpoint (`ep:<name>`), or profile.

```sh
agyva use personal              # alias: switch
agyva use 2
agyva use -                     # previous OAuth account
agyva use ep:openai             # activate a custom endpoint
agyva use --profile work        # activate a profile's account and/or endpoint
```

Selectors accept an alias, list number, exact email, account id, unique id
prefix, `ep:<name>` for custom endpoints, or `-` for the previous OAuth
account. Ambiguous selectors fail with exit 2. Explicit target flags
`--account`, `--endpoint`, and `--profile` avoid ambiguity.

If agy holds an unsaved login, save it with `add` first; `use <selector> --force`
replaces it only after keeping a pinned encrypted checkpoint, which you can later
inspect or restore with [`agyva checkpoints`](#checkpoints). Switching to an OAuth
account clears any active custom endpoint (`modelProvider` / `customModelsConfig`
in `agy`'s `settings.json`).

**Restart `agy` after switching.** Existing sessions keep their old login;
Agyva reports detected sessions and never kills them. A switch never happens
automatically. Exit code 3 and the recovery procedure are in
[Recover an interrupted switch](#recover-an-interrupted-switch).

## Custom endpoints, bridge, env, and exec

Manage custom LLM providers, translate protocols locally, and run tools with
resolved endpoint or profile settings.

```sh
agyva endpoint presets                # alias: ep
agyva endpoint add openai --preset openai --api-key-env OPENAI_API_KEY --use
agyva endpoint list
agyva endpoint show openai
agyva endpoint test openai
agyva endpoint models openai --refresh
agyva endpoint update openai --model gpt-4.1
agyva endpoint rename openai prod-openai
agyva endpoint use --clear
agyva endpoint remove prod-openai -y
```

API keys are read from `--api-key-env <VAR>`, `--api-key-stdin`, or an
interactive hidden prompt (never argv), and stored encrypted in
`<config>/endpoints/<id>.ep.snap`. Non-secret metadata lives in
`<config>/endpoints.json`, and discovered models/rate limits live in
`<cache>/endpoint-models.json`.

- **17 built-in presets**: `openai`, `anthropic`, `gemini`, `openrouter`, `groq`, `deepseek`, `mistral`, `together`, `fireworks`, `xai`, `perplexity`, `ollama`, `lmstudio`, `vllm`, `litellm`, `azure`, `cohere`.
- **Wire protocols** (`--protocol`): `auto` (default when no preset sets one), `openai`, `openai-responses`, `anthropic`, `gemini`, `azure-openai`, `ollama`, `cohere`, `custom`.
- **Auth schemes** (`--auth-scheme`): `bearer`, `x-api-key`, `x-goog-api-key`, `api-key`, `none`, or `custom-header:<name>[:<prefix>]`.
- **URL validation**: `https://` is required for remote hosts; `http://` is permitted only for loopback or local addresses (`localhost`, `*.localhost`, `*.local`, `*.internal`, loopback IPs, RFC1918 private IPv4, `fc00::/7` IPv6). Embedded URL credentials (`user:pass@`) are rejected.
- **`agy` sync**: `agyva endpoint use <sel>` and `agyva use ep:<sel>` write `modelProvider` and `customModelsConfig` into `agy`'s `settings.json`, preserving other keys and never writing API keys.

### Local protocol bridge, env, and exec

```sh
agyva bridge --endpoint openai --port 8787
agyva env --endpoint openai --shell bash
agyva exec --endpoint openai --bridge -- my-tool --flag
```

- **`agyva bridge`** starts a synchronous loopback HTTP/1.1 server (`--listen 127.0.0.1:0` by default, `--port` override, optional `--local-token-env`) accepting OpenAI Chat (`/v1/chat/completions`), Anthropic Messages (`/v1/messages`), Gemini (`/v1beta/models/{model}:generateContent` and `:streamGenerateContent`), and model-listing routes (`/v1/models`, `/v1beta/models`, `/api/tags`), translating requests to the endpoint's configured protocol.
- **`agyva env`** prints shell-quoted exports (`--shell bash|zsh|fish|powershell|dotenv`) for `--endpoint`, `--profile` (or auto-discovered profile / active endpoint), optional `--model`, and optional `--bridge-url`.
- **`agyva exec`** injects resolved profile, account (`AGYVA_ACCOUNT`, `AGYVA_ACCOUNT_ID`), and endpoint environment variables into a child command (`-- <COMMAND> [ARGS...]`). Passing `--bridge` starts an ephemeral `127.0.0.1:0` bridge for the target endpoint and tears it down when the command exits.

## Project and global profiles

Store non-secret workflow defaults (account, endpoint, routing group, model,
thresholds, monitor interval, and non-secret `env` pairs) in a project-local
`.agyva.toml` or global `<config>/profiles.toml`.

```sh
agyva profile init --local --endpoint openai --model gpt-4o   # alias: prof
agyva profile create work --account work --group work --warn-below 30 --default
agyva profile list
agyva profile which
agyva profile show work
agyva profile set work model claude-sonnet-4-20250514
agyva profile use work
agyva profile delete work -y
```

Without `--profile`, Agyva walks up from the current directory looking for
`.agyva.toml` (stopping at `.git`, filesystem root, or 32 levels), then falls
back to `config.default_profile`, then `default` in `profiles.toml`. Environment
keys containing `API_KEY`, `SECRET`, `TOKEN`, `PASSWORD`, or `PASSPHRASE` are
refused; keep credentials in the vault or encrypted endpoint snapshots.

## Inspect quota and health

Read cached evidence for one account or all of them; only the active account can
refresh.

```sh
agyva usage                     # active account; normal refresh
agyva usage --all --compact     # compact table; others use cache
agyva usage work --no-refresh   # cached data only
agyva statusline                # fast cache-only status bar summary
agyva health --no-refresh       # health for every saved account
agyva watch --warn-below 25 --critical-below 10
```

| Flag | Effect |
| --- | --- |
| `--all` | Include every saved account (`usage`, `monitor`). |
| `--compact` | Single-line-per-account table (`usage`). |
| `--profile NAME` | Apply account/defaults from a named profile (`usage`, `statusline`, `watch`, `monitor`). |
| `--no-refresh` | Read the cache; never run agy (`usage`, `health`). |
| `--force-refresh` | Ignore the minimum interval and check a suspended account once; conflicts with `--no-refresh` (`usage`, `health`). |
| `--warn-below PCT` / `--critical-below PCT` | Quota threshold alerts (`watch`, `monitor`). |
| `--once` | Run a single cycle and exit (`watch`, `monitor`). |
| `--json` | Machine-readable output — see [JSON and exit codes](json.md). |
| `--json-stream` | Emit one NDJSON event object per line (`watch`, `monitor`). |

Normal refreshes respect the minimum interval and skip suspended accounts; only
the active account can refresh. Quota observations are recorded in
`<cache>/quota-history.json` to compute per-window burn rates (`%/h`) and
exhaustion ETAs. `watch` observes the active account at the configured interval,
backs off on transient failures, emits threshold alerts (`--warn-below`,
`--critical-below`, or config/profile defaults), and never switches or warms
quota windows. Optional desktop notifications (`--notify` or
`desktop_notifications = true`) contain an alias and state, not credentials.
[Windows, groups and freshness](antigravity.md#read-quota-and-health). Model
catalogs have their own command: [Model catalog](#model-catalog).

### Statusline

`agyva statusline` prints a fast, cache-only summary suitable for shell prompts
and status bars without spawning `agy` or touching the keyring.

```sh
agyva statusline --format compact   # compact | full | plain | starship | tmux | waybar | json
```

Defaults come from `statusline_format` in `config.toml` (`compact` by default)
and thresholds from the active `--profile` or `config.toml`.

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
the gate.

### Route-aware monitor

Print the sticky pick plus each account’s share, freshness, burn rate/ETA, and
exclusion reason, with the next weekly reset countdown.

```sh
agyva monitor [--interval 15s] [--group NAME | --all] [--for-model FAMILY] [--profile NAME]
```

Alias: `agyva mon`. Every tick re-ranks the cached evidence (never an agy spawn).
`--interval` accepts `15s`, `30`, `2m`, `1h` with a 5 s floor — a faster loop
changes nothing, because the evaluation is cache-only. `--once` runs one cycle,
`--json` prints a single `MonitorSnapshot` (implies `--once`), and
`--json-stream` emits continuous NDJSON events with snapshots. The active
account’s quota refresh runs on its own 300 s cadence through the existing
single-flight refresh guard, so several monitors and a manual `usage` collapse
to one run; the same tick keeps the active account’s model catalog current
(bounded `agy models`, throttled by `models_cache_secs`, skipped while the
observation is fresh). `monitor` never switches accounts unless the group
explicitly opts in — see [Opt-in auto-switch](#opt-in-auto-switch). You can also
inspect and apply route groups or profiles interactively in the dashboard’s
`Routes` tab (`F4`).

## Plan a route

Create routing groups, rank cached readings without running agy or changing any
account, and optionally activate the recommendation.

```sh
agyva route init [-y]                            # seed `default` group from saved accounts
agyva route add work --accounts work,personal    # create/update a group (--ranking, --auto-switch)
agyva route remove work [-y]                     # remove a group
agyva route list                                 # groups and their activation mode
agyva route inspect GROUP                        # cached evidence per account
agyva route plan GROUP [--json]                  # recommended account
agyva route plan                                 # every saved account, no group
agyva route apply GROUP [--yes]                  # one journaled switch
agyva route history [--limit N]                  # recent decisions and reasons
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

Finish or undo a switch that did not complete, or inspect and repair local state.

```sh
agyva status          # alias: st; state and any pending recovery
agyva doctor          # read-only checks
agyva doctor --fix    # apply safe non-destructive repairs (0700/0600 modes, dirs, stale lockfiles)
agyva recover         # complete a verified switch, or restore the checkpoint
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

`agyva doctor --fix` ensures 0700/0600 permissions on private directories and
files (under Unix), creates missing state/cache subdirectories when already
initialized, and removes uncontended stale lockfiles whose recorded PID is no
longer running, then re-runs `doctor`.

### Checkpoints

List, restore, or prune encrypted pre-switch checkpoints (ordinary `*.ckpt` and
pinned `*.keep.ckpt`).

```sh
agyva checkpoints list                         # alias: ckpt
agyva checkpoints restore <NAME> [--force] [-y]
agyva checkpoints prune [--include-pinned] [-y]
```

`restore` refuses when recovery is pending (exit 3). If the live `agy` login
differs from the checkpoint, `--force` keeps an encrypted pinned backup of the
current login before restoring, then clears any active custom endpoint in
`endpoints.json`. `prune` removes ordinary checkpoints beyond `checkpoints_keep`;
pass `--include-pinned` to prune pinned `*.keep.ckpt` copies too.

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

Check for a newer stable release, or download, verify, and install it.

```sh
agyva update --check              # check and report only; installs nothing
agyva update                      # check, verify, download and install
agyva update --install            # same as `agyva update`
```

`agyva update --check` queries GitHub Releases and reports whether a newer
version exists without modifying anything; `agyva update` (or
`agyva update --install`) downloads, verifies, and replaces the running binary
atomically. Fresh installs can also use [`install.sh`](../install.sh) on Linux
(x86-64 and ARM64) and macOS (x86-64 and ARM64).

Updates come from this project’s GitHub Releases on supported targets
(`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`,
`aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, and
`aarch64-pc-windows-msvc`): the standalone release binary and its `SHA256SUMS`
are verified before the running binary is replaced atomically. Every human-output
startup also checks for a newer stable release (2 s timeout, silent on failure,
cached in `<cache>/update-check.json` for 24 hours on success and 1 hour on
failure; skipped on unsupported targets and for `--json` or `completions`) and
prints a one-line notice on stderr; disable that with `check_updates = false` in
the config or `AGYVA_UPDATE_CHECK=0` in the environment.
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
the file path; `agyva config set <key> <value>` validates before saving;
`agyva config reset <key>` restores a single setting to its default.

| Setting | Default / range |
| --- | --- |
| `allow_untested_agy` | `false`; accept agy versions outside the known-good range? |
| `auto_switch_ack` | `false`; explicit acknowledgement that opt-in route automation may switch accounts. |
| `auto_switch_candidate_max_age_secs` | `21600`; maximum cache age in seconds for standby auto-switch candidates, 60–604800. |
| `auto_switch_cooldown_secs` | `900`; minimum seconds between switches before auto-switch may run again, 30–86400. |
| `backend` | `auto`; `secret-service` (aliases `keyring`, `keychain`, `credential-manager`) or `file` chooses agy’s native store. |
| `check_updates` | `true`; check GitHub Releases for newer agyva at startup. |
| `checkpoints_keep` | `5`; ordinary checkpoints retained, 1–20. Pinned copies are never pruned. |
| `color` | `auto`; `always` or `never`. Nonempty `NO_COLOR` disables colors; pipes stay plain. |
| `critical_percent` | `10`; remaining quota critical threshold percentage (0–100, `<= warn_percent`). |
| `default_profile` | unset; default global profile name (or empty string to unset). |
| `desktop_notifications` | `false`; let `agyva watch` send desktop notifications. |
| `lock_timeout_secs` | `10`; seconds to wait for another agyva, 0–600. |
| `models_cache_secs` | `86400`; how long a saved model catalog counts as fresh, 60–604800. |
| `policy_acknowledged` | `false`; whether the one-time policy notice was shown. |
| `quota_fresh_secs` | `300`; how long a quota reading counts as fresh, 10–86400. |
| `quota_min_refresh_secs` | `60`; minimum seconds between quota refreshes, 10–3600. |
| `statusline_format` | `compact`; `compact`, `full`, or `json`. |
| `vault_key` | `auto`; `keyring` or `passphrase`. Sets initial vault mode; use `agyva rekey` to rotate or migrate an existing vault. |
| `warn_percent` | `25`; remaining quota warning threshold percentage (0–100). |
| `watch_interval_secs` | `300`; seconds between `agyva watch` checks, 60–86400. |

State and cache use platform config/state/cache directories (XDG on Linux).
`AGYVA_LOG=debug` sends logs to stderr; logging is disabled in the dashboard.
Development overrides and isolated backends are in
[development](development.md#environment-overrides).

## Shell completion

Print a completion script for your shell (`fish`, `bash`, `zsh`, `powershell`,
or `elvish`).

```sh
mkdir -p ~/.config/fish/completions
agyva completions fish > ~/.config/fish/completions/agyva.fish

mkdir -p ~/.local/share/bash-completion/completions
agyva completions bash > ~/.local/share/bash-completion/completions/agyva

mkdir -p ~/.zfunc
agyva completions zsh > ~/.zfunc/_agyva

agyva completions powershell > agyva.ps1
agyva completions elvish > agyva.elv
```

For Zsh, add `~/.zfunc` to `$fpath` in your shell configuration. Fish also
completes saved account aliases, `ep:<endpoint>` targets, profile names, and
checkpoint names by running read-only `--json` subcommands that need no keyring
access.
