# Dashboard

[README](../README.md) · [Commands](commands.md) · [Quota and health](antigravity.md#read-quota-and-health)

Run `agyva` in a terminal. The dashboard has five tabs (`Tab` / `Shift+Tab` or
`F1`–`F5`): **Accounts** (compact overview and summary strip), **Usage**
(per-group cards, gauges, and burn-rate ETA), **Endpoints** (custom LLM
endpoints and live probe/fetch), **Routes** (active profile, saved profiles,
and `routing.toml` groups), and **Diagnostics** (environment cards and `doctor`
checks). `?` opens the key reference.

## Read the overview

`●` marks the active account (or active endpoint / profile on their tabs).
Reverse video marks the selected row, including with colors disabled. Health
carries text and alarming states also carry `!`.

On wide terminals (`>=60` columns and `>=12` rows), **Accounts** renders a
Summary Metric Strip (`Active Target`, `Lowest & Burn`, and on `>=96` columns
also `Endpoints` and `Next Reset`) above a rounded table container. Smooth
block gauges (`█▓░`) show remaining percentages.

The **5H LEFT** and **WEEKLY LEFT** columns show the minimum reported remaining
percentage for that window across enabled groups. They are a conservative
summary, not a shared account-wide quota. `—` means the window was not reported.
A group that is not enabled is excluded from the minimum; exhausted groups
remain included. Open **Usage** (`F2`) to see which group is limiting, along
with per-window burn rate (`%/h`) and exhaustion ETA.

**CACHE / AGE** reports freshness and observation age. It is not a subscription
plan; agy’s usage response does not report plans. **BURN ETA** estimates time to
exhaustion from `<cache>/quota-history.json`. As the terminal narrows, columns
drop in order: `BURN ETA`, `CACHE / AGE`, `WEEKLY LEFT`, `STATUS`, `5H LEFT`.
Alias, selection and help/quit controls remain visible.

## Endpoints and Routes tabs

- **Endpoints (`F3`)**: Lists configured custom endpoints with protocol badges
  (`[OPENAI]`, `[RESPONSES]`, `[ANTHROPIC]`, `[GEMINI]`, `[AZURE]`, `[OLLAMA]`,
  `[COHERE]`, `[CUSTOM]`, `[AUTO]`), latency, cached model count, default model,
  and rate-limit/credit summary or cached error. Press `t` to test connectivity,
  `f` to fetch and cache models, `d` for details (including cached models and
  rate limits), or `Enter` to activate the endpoint and sync `agy`'s
  `settings.json`.
- **Routes (`F4`)**: Shows the active project/global profile banner, saved
  local (`.agyva.toml`) and global (`profiles.toml`) profiles, and `routing.toml`
  groups with their ranked candidates and exclusion reasons. Press `Enter` on a
  profile to activate its account/endpoint, or `Enter` on a route group to
  confirm and apply its recommended account.

## Keys

| Key | Action |
| --- | --- |
| `Tab`, `Shift+Tab`, `F1`–`F5` | Change view (`Accounts`, `Usage`, `Endpoints`, `Routes`, `Diagnostics`). |
| `↑↓`, `j/k` | Select an item in Accounts/Usage/Endpoints/Routes; scroll Diagnostics. |
| `1`–`9` | Quick-select a list number. With 10+ items, use arrows or `/`. |
| `Enter` | Open confirmation to switch account (Accounts/Usage), activate endpoint (Endpoints), or apply route/profile (Routes). No Enter action in Diagnostics. |
| `y` | Confirm a pending action. |
| `n`, `Esc`, `q` in confirmation | Cancel without making changes. |
| `s` | Cycle sort order on Accounts (`default`, `quota`, `alias`, `freshness`). |
| `t`, `f` | Test connection (`t`) or fetch models (`f`) for the selected endpoint on Endpoints. |
| `r` | Normal refresh of the active account, respecting interval/suspension restrictions. |
| `d` | Context-sensitive details (Accounts/Usage, Endpoints, or Routes). |
| `/` | Filter items (Accounts, Usage, Endpoints, Routes; disabled in Diagnostics). Enter keeps the filter; Esc clears. |
| `?` | Help, including while editing a filter. |
| `PageUp/Down`, `Home/End` | Scroll Usage/Diagnostics; also Help, Details and confirmation content. |
| `↑↓`, `j/k` in a window | Scroll wrapped content. |
| `Esc`, `q` in Help/Details | Close the window. |
| `q`, `Esc` from a view | Quit; Esc clears an existing filter first. |
| `Ctrl+C` | Quit from any state, including filter entry. |

While entering a filter, ordinary letters (including `q`) become filter text;
the footer shows `^C quit` and `? help`. Outside filter entry it shows `q quit`,
including while an error or a retained filter is visible.

Help and Details scroll on short terminals. Confirmation reserves its final
row for `y` and `Esc`, so wrapped warnings cannot hide cancellation. At very
small sizes content is compact; enlarge the terminal to read more at once.

## Refresh and switch behavior

Opening the dashboard loads the cache first and then requests one normal
refresh. `r` requests another if none is running. Only the active account can
refresh; other accounts retain their cache and its age. No periodic refresh
runs merely because the dashboard stays open. Relative times and freshness
are recalculated every 30 seconds using `quota_fresh_secs`.

Enter opens a confirmation with restart consequences. A switch uses the same
journaled path as `agyva use`, never `--force`; an unsaved live login is not
silently replaced. **Restart agy after switching.** Running sessions keep their
old login. A quit during a switch waits for the switch to finish. After switching,
the dashboard rereads the cache without running `/usage`.

A passphrase vault cannot prompt while the dashboard owns the terminal. Use
`agyva use` in a shell, or supply the existing passphrase through
`AGYVA_PASSPHRASE` with the [documented environment tradeoff](security.md#what-is-protected).

`color = never` or a nonempty `NO_COLOR` removes foreground/background colors;
bold, reverse selection, symbols and text states remain. `NO_COLOR=` is empty
and does not disable the configured behavior. The UI uses the terminal’s ANSI
palette when color is enabled.

## Capture provenance

![Actual cached quota rendered by the final Usage view](tui-usage.svg)

Captured from the installed Agyva’s existing account registry and quota cache;
identifying fields replaced with samples. Quotas and timestamps preserved.
No refresh or live login verification performed.

The [sanitized input](capture-data/accounts.json) contains two sample identities,
`personal@example.invalid` and `work@example.invalid`, with independent sample
ids. Both captured readings are stale and quota limited. Gemini weekly
remaining is `98.9199995994568%` and `100%` respectively; Claude and GPT models
have `0%` weekly remaining and are exhausted. Neither account reports a five-hour
window, model subtype or plan. All original observation/reset timestamps remain
in the fixture. Display percentages are rounded; JSON retains the measurements.

The [Accounts capture](tui.svg) therefore shows `0%` in the weekly minimum
column for both accounts. It does not mean Gemini is exhausted. The
[text capture](tui-capture.txt) includes Accounts, Usage for both accounts,
Details, Help, a narrow view and short confirmation/help windows.

`scripts/capture-tui.sh` rebuilds the debug binary and a capture helper, replays
only registry/cache data into scratch directories, and renders the production
views with Ratatui’s `TestBackend`. SVG cell positions and selection come from
the rendered buffer. This is a rendered capture rather than a PTY recording;
interactive terminal navigation, switching, Ctrl+C and restoration are checked
separately with synthetic credentials in the PTY tests. The replay does not
contain usable credentials and never refreshes or verifies a live login.
Relative ages change when regenerated; measurements and timestamps do not.

For made-up demo data instead, use `scripts/capture-tui.sh --synthetic /tmp/demo.txt`.
Its output is explicitly marked synthetic. [Capture implementation](development.md#scripts).
