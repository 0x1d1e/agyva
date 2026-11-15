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
```

`usage` and `health` accept a selector, `--json`, `--no-refresh`, and
`--force-refresh`. `usage` also accepts `--all`. Normal refreshes respect the
minimum interval and skip suspended accounts; `--force-refresh` overrides both
for one explicit check. It conflicts with `--no-refresh`. Only the active
account can refresh. [Windows, groups and freshness](antigravity.md#read-quota-and-health).

`watch` observes the active account at the configured interval, backs off on
transient failures, and never switches or warms quota windows. Optional desktop
notifications contain an alias and state, not credentials.

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
| `quota_fresh_secs` | `300`; 10–86400. |
| `quota_min_refresh_secs` | `60`; 10–3600. |
| `watch_interval_secs` | `300`; 60–86400. |
| `desktop_notifications` | `false`. |

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
