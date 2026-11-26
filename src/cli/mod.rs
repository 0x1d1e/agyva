//! Command-line definition (clap) and dispatch.

mod auto_cmd;
mod commands;
mod endpoints_cmd;
pub mod monitor_cmd;
pub mod output;
mod profiles_cmd;
pub mod usage_out;
mod watch_cmd;
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

pub use commands::{Io, run};

const EXIT_CODES: &str = "\
Examples:
  agyva add --alias personal       Save the current agy login
  agyva usage --all --no-refresh   Inspect cached quota without running agy
  agyva use work                   Switch manually, then restart agy
  agyva endpoint add openai --preset openai --api-key-env OPENAI_API_KEY
  agyva exec --endpoint openai -- my-tool
  agyva                            Open the terminal dashboard

Exit codes:
  0  success
  1  error (the message goes to stderr, followed by a `hint:` line)
  2  usage error (bad arguments, selector or setting)
  3  recovery required: an interrupted switch or import is pending, or a
     keyring write timed out and its outcome is unknown (run `agyva recover`)

Selectors (for use, rename, remove): an alias, a list number, an exact email,
an account id or unique id prefix, `ep:<name>` for custom endpoints (in `use`),
or `-` for the previous account.

Environment (development and tests): AGYVA_CONFIG_DIR, AGYVA_STATE_DIR,
AGYVA_CACHE_DIR, AGYVA_AGY_HOME, AGYVA_AGY_BIN, AGYVA_PASSPHRASE,
AGYVA_NEW_PASSPHRASE, AGYVA_LOG, AGYVA_VAULT_KEY. AGYVA_BACKEND=memory|file
(never touches the OS keyring) needs AGYVA_CONFIG_DIR, AGYVA_STATE_DIR and
AGYVA_AGY_HOME as well. AGYVA_ARCHIVE_PASSPHRASE protects export/import
archives (separate from the vault passphrase). AGYVA_FAILPOINT works only
with AGYVA_TEST_MODE=1.";

#[derive(Debug, Parser)]
#[command(
    name = "agyva",
    version,
    about = "Account manager for Google's Antigravity CLI (agy)",
    long_about = "Save agy logins in an encrypted vault and switch between them on request.\n\
                  Manage custom LLM endpoints, local protocol bridge, and project profiles.\n\
                  Switching is manual (`agyva use`) unless you run `agyva auto` or a route\n\
                  group opts in (auto_switch = true plus `config set auto_switch_ack true`).",
    after_help = EXIT_CODES,
    help_template = "{name} {version}\n{about-with-newline}\n{usage-heading} {usage}\n\nAccounts:\n  add          Save the current agy login\n  list (ls)    List saved accounts\n  use (switch) Switch account, `ep:<name>`, or `--profile`\n  current      Inspect agy's login\n  rename       Change an alias\n  remove       Forget a saved account\n\nCustom endpoints and workflows:\n  endpoint (ep) Manage custom endpoints and presets\n  bridge       Start a local protocol-translating HTTP bridge\n  env          Print shell exports for an endpoint or profile\n  exec         Run a command with endpoint/profile env (and optional --bridge)\n  profile (prof) Manage local `.agyva.toml` and global profiles\n\nQuota and health:\n  usage        Remaining quota by group and window\n  statusline   Fast cache-only summary for prompts and status bars\n  health       Account health and evidence\n  models       Cached model catalog (--refresh to update)\n  watch        Read-only foreground monitor\n  monitor (mon) Route-aware cache-only monitor (--interval)\n  auto         Rotate one model family when usage is over 90% (restart agy)\n\nRouting:\n  route        Rank saved accounts from cached quota (--for-model, apply)\n\nDiagnostics and recovery:\n  status (st)  State and pending recovery\n  doctor       Diagnose problems (--fix for safe repairs)\n  checkpoints (ckpt) List, restore, or prune encrypted checkpoints\n  recover      Finish or undo an interrupted switch\n\nTransfer and updates:\n  export       Save accounts to a portable encrypted archive\n  import       Add accounts from an archive (skips existing)\n  update       Check GitHub for a newer release; install on request\n\nSetup:\n  config       View, change, or reset settings\n  rekey        Rotate the vault key or passphrase\n  completions  Fish, Bash, Zsh, PowerShell or Elvish completion\n  version      agyva and agy versions\n  help         Help for a command\n\nOptions:\n{options}\n{after-help}",
    propagate_version = true
)]
pub struct Cli {
    /// Allow agy versions that agyva has not been verified against.
    #[arg(long, global = true)]
    pub allow_untested: bool,

    /// Suppress non-essential notices and advisory output.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Save the account agy is currently logged in as.
    Add(AddArgs),
    /// List saved accounts.
    #[command(alias = "ls")]
    List(JsonFlag),
    /// Switch agy to a saved account, custom endpoint (`ep:<name>`), or profile.
    #[command(alias = "switch")]
    Use(UseArgs),
    /// Show which account agy is logged in as.
    Current(JsonFlag),
    /// Change an account's alias.
    Rename(RenameArgs),
    /// Forget a saved account (agy's own login is never touched).
    Remove(RemoveArgs),
    /// Manage custom LLM endpoints and provider presets.
    #[command(alias = "ep")]
    Endpoint(EndpointArgs),
    /// Start a local HTTP/1.1 loopback protocol bridge for a custom endpoint.
    Bridge(BridgeArgs),
    /// Print shell-quoted environment exports for an endpoint or profile.
    Env(EnvArgs),
    /// Execute a command with resolved endpoint/profile environment variables.
    Exec(ExecArgs),
    /// Manage project-local (`.agyva.toml`) and global profiles.
    #[command(alias = "prof")]
    Profile(ProfileArgs),
    /// Fast, cache-only status summary for shell prompts and status bars.
    Statusline(StatuslineArgs),
    /// Show quota usage (refreshes the active account; others come from the cache).
    Usage(UsageArgs),
    /// Rank saved accounts from cached quota without running agy.
    Route(RouteArgs),
    /// Monitor usage and health in the foreground (read-only; never switches).
    Watch(WatchArgs),
    /// Route-aware monitor: cache-only ranking per interval, single-flight
    /// active refresh, never switches by itself.
    #[command(alias = "mon")]
    Monitor(MonitorArgs),
    /// Rotate one model family when usage is over 90% (restart agy).
    Auto(AutoArgs),
    /// Show per-account health (refreshes the active account).
    Health(HealthArgs),
    /// Show the cached model catalog (`agy models`); `--refresh` runs it now.
    Models(ModelsArgs),
    /// Summarize agyva's state and any pending recovery.
    #[command(alias = "st")]
    Status(JsonFlag),
    /// Diagnose problems (read-only by default; `--fix` applies safe repairs).
    Doctor(DoctorArgs),
    /// List, restore, or prune encrypted pre-switch checkpoints.
    #[command(alias = "ckpt")]
    Checkpoints(CheckpointsArgs),
    /// Finish or undo an interrupted switch.
    Recover(RecoverArgs),
    /// View, change, or reset settings.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Rotate the vault key or passphrase, or migrate between keyring and passphrase.
    Rekey(RekeyArgs),
    /// Print a shell completion script.
    Completions {
        #[arg(value_enum)]
        shell: ShellName,
    },
    /// Save accounts to a portable, password-encrypted archive.
    Export(ExportArgs),
    /// Add accounts from an archive (existing same identities are skipped).
    Import {
        /// Archive file to import.
        file: std::path::PathBuf,
    },
    /// Check GitHub Releases for a newer agyva (--check) or download and install it.
    Update {
        /// Only check and report; do not download or install anything.
        #[arg(long)]
        check: bool,
        /// Download and replace this binary with the newer release.
        #[arg(long, conflicts_with = "check")]
        install: bool,
    },
    /// Print agyva and agy versions.
    Version,
}

#[derive(Debug, Args)]
pub struct ModelsArgs {
    /// Run `agy models` for the active account instead of reading the cache.
    #[arg(long)]
    pub refresh: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct JsonFlag {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Apply safe non-destructive fixes (0700/0600 permissions, stale lockfiles).
    #[arg(long)]
    pub fix: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct CheckpointsArgs {
    #[command(subcommand)]
    pub command: CheckpointsCommand,
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum CheckpointsCommand {
    /// List saved checkpoints (newest first).
    List,
    /// Restore a saved checkpoint by filename.
    Restore {
        /// Checkpoint filename (see `agyva checkpoints list`).
        name: String,
        /// Keep an encrypted pinned backup if the live login differs.
        #[arg(long)]
        force: bool,
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Prune old checkpoints beyond `checkpoints_keep`.
    Prune {
        /// Also prune pinned `.keep.ckpt` checkpoints.
        #[arg(long)]
        include_pinned: bool,
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct EndpointArgs {
    #[command(subcommand)]
    pub command: EndpointCommand,
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum EndpointCommand {
    /// List built-in provider presets.
    Presets,
    /// Save a new custom endpoint with an encrypted API key.
    Add(EndpointAddArgs),
    /// List saved custom endpoints.
    List,
    /// Show detailed metadata and cached models for an endpoint.
    Show { selector: String },
    /// Activate a custom endpoint (or `--clear` to deactivate).
    Use {
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        selector: Option<String>,
        /// Deactivate the active custom endpoint.
        #[arg(long)]
        clear: bool,
    },
    /// Probe endpoint connectivity and model listing without modifying cache.
    Test { selector: String },
    /// Show cached models for an endpoint (`--refresh` fetches live).
    Models {
        selector: String,
        /// Fetch models from the endpoint and update the cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Update an existing endpoint's settings or API key.
    Update(EndpointUpdateArgs),
    /// Rename an endpoint's alias.
    Rename { selector: String, new_alias: String },
    /// Remove a saved custom endpoint.
    Remove {
        selector: String,
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct EndpointAddArgs {
    /// Alias for the custom endpoint.
    pub alias: String,
    /// Start from a built-in provider preset (see `agyva endpoint presets`).
    #[arg(long)]
    pub preset: Option<String>,
    /// Upstream base URL (`https://...` or loopback/local `http://...`).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Wire protocol (`auto`, `openai`, `openai-responses`, `anthropic`, `gemini`, `azure-openai`, `ollama`, `cohere`, `custom`).
    #[arg(long)]
    pub protocol: Option<String>,
    /// Auth header scheme (`bearer`, `x-api-key`, `x-goog-api-key`, `api-key`, `none`, `custom-header:<name>[:<prefix>]`).
    #[arg(long)]
    pub auth_scheme: Option<String>,
    /// Default model id for this endpoint.
    #[arg(long)]
    pub model: Option<String>,
    /// API version (used by Azure OpenAI).
    #[arg(long)]
    pub api_version: Option<String>,
    /// Custom models discovery path.
    #[arg(long)]
    pub models_path: Option<String>,
    /// Custom chat completions path.
    #[arg(long)]
    pub chat_path: Option<String>,
    /// Extra non-secret HTTP header (`KEY=VAL`; repeatable).
    #[arg(long = "header", value_name = "KEY=VAL")]
    pub headers: Vec<String>,
    /// Read the API key from standard input.
    #[arg(long, conflicts_with = "api_key_env")]
    pub api_key_stdin: bool,
    /// Read the API key from the named environment variable.
    #[arg(long, value_name = "VAR", conflicts_with = "api_key_stdin")]
    pub api_key_env: Option<String>,
    /// Skip automatic model discovery after adding.
    #[arg(long)]
    pub no_fetch: bool,
    /// Activate this endpoint immediately after adding.
    #[arg(long, name = "use")]
    pub activate: bool,
}

#[derive(Debug, Args)]
pub struct EndpointUpdateArgs {
    /// Endpoint alias, number, or id (prefix).
    pub selector: String,
    /// Update upstream base URL.
    #[arg(long)]
    pub base_url: Option<String>,
    /// Update wire protocol.
    #[arg(long)]
    pub protocol: Option<String>,
    /// Update auth header scheme.
    #[arg(long)]
    pub auth_scheme: Option<String>,
    /// Update default model id.
    #[arg(long, conflicts_with = "clear_model")]
    pub model: Option<String>,
    /// Clear the default model id.
    #[arg(long, conflicts_with = "model")]
    pub clear_model: bool,
    /// Update API version.
    #[arg(long)]
    pub api_version: Option<String>,
    /// Update custom models path.
    #[arg(long)]
    pub models_path: Option<String>,
    /// Update custom chat path.
    #[arg(long)]
    pub chat_path: Option<String>,
    /// Replace/add extra HTTP header (`KEY=VAL`; repeatable).
    #[arg(long = "header", value_name = "KEY=VAL")]
    pub headers: Vec<String>,
    /// Clear all extra HTTP headers.
    #[arg(long)]
    pub clear_headers: bool,
    /// Read a new API key from standard input.
    #[arg(long, conflicts_with = "api_key_env")]
    pub api_key_stdin: bool,
    /// Read a new API key from the named environment variable.
    #[arg(long, value_name = "VAR", conflicts_with = "api_key_stdin")]
    pub api_key_env: Option<String>,
    /// Mark endpoint as enabled.
    #[arg(long, conflicts_with = "disable")]
    pub enable: bool,
    /// Mark endpoint as disabled.
    #[arg(long, conflicts_with = "enable")]
    pub disable: bool,
}

#[derive(Debug, Args)]
pub struct BridgeArgs {
    /// Custom endpoint alias, number, or id (prefix).
    #[arg(long)]
    pub endpoint: String,
    /// Loopback listen address (`host:port`).
    #[arg(long, default_value = "127.0.0.1:0")]
    pub listen: String,
    /// Override the listen port (e.g. `8787`).
    #[arg(long)]
    pub port: Option<u16>,
    /// Environment variable holding a local bearer token. When set, every
    /// non-OPTIONS request must send `Authorization: Bearer <value>` or the
    /// bridge returns 401 and does not forward. OPTIONS stays open and is
    /// not forwarded. Omit the flag to keep loopback-only open access.
    #[arg(long, value_name = "VAR")]
    pub local_token_env: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EnvShell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Dotenv,
}

#[derive(Debug, Args)]
pub struct EnvArgs {
    /// Custom endpoint selector.
    #[arg(long, conflicts_with = "profile")]
    pub endpoint: Option<String>,
    /// Profile name.
    #[arg(long, conflicts_with = "endpoint")]
    pub profile: Option<String>,
    /// Override model id (`AGYVA_MODEL`).
    #[arg(long)]
    pub model: Option<String>,
    /// Override target URL with a running bridge URL.
    #[arg(long)]
    pub bridge_url: Option<String>,
    /// Output shell syntax.
    #[arg(long, value_enum, default_value_t = EnvShell::Bash)]
    pub shell: EnvShell,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Profile name to apply.
    #[arg(long)]
    pub profile: Option<String>,
    /// Account selector to activate before running.
    #[arg(long)]
    pub account: Option<String>,
    /// Custom endpoint selector whose env vars are injected.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Override model id (`AGYVA_MODEL`).
    #[arg(long)]
    pub model: Option<String>,
    /// Start an ephemeral `127.0.0.1:0` protocol bridge for the target endpoint.
    #[arg(long)]
    pub bridge: bool,
    /// Command and arguments to run (`-- <COMMAND> [ARGS...]`).
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub command: ProfileCommand,
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Initialize a local `.agyva.toml` (default) or global named profile.
    Init(ProfileInitArgs),
    /// Create a new global profile.
    Create(ProfileCreateArgs),
    /// List global profiles and any discovered local `.agyva.toml`.
    List,
    /// Show the resolved active profile or a named profile.
    Show { name: Option<String> },
    /// Show which profile resolves in the current directory.
    Which,
    /// Set a field (`account`, `endpoint`, `group`, `model`, `warn_percent`, `critical_percent`, `monitor_interval`, `description`, `name`, `env.<KEY>`).
    Set(ProfileSetArgs),
    /// Delete a global profile.
    Delete {
        name: String,
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Activate the account and/or endpoint configured by a profile.
    Use {
        name: Option<String>,
        /// Replace an unsaved live agy login when switching accounts.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub struct ProfileFieldsArgs {
    /// Default account selector (or `ep:<name>`).
    #[arg(long)]
    pub account: Option<String>,
    /// Default custom endpoint selector.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Default routing group.
    #[arg(long)]
    pub group: Option<String>,
    /// Default model id.
    #[arg(long)]
    pub model: Option<String>,
    /// Warning quota threshold percentage (0-100).
    #[arg(long, value_name = "PCT")]
    pub warn_below: Option<f64>,
    /// Critical quota threshold percentage (0-100).
    #[arg(long, value_name = "PCT")]
    pub critical_below: Option<f64>,
    /// Default monitor interval (e.g. `15s`, `1m`).
    #[arg(long)]
    pub monitor_interval: Option<String>,
    /// Profile description.
    #[arg(long)]
    pub description: Option<String>,
    /// Non-secret environment variable (`KEY=VAL`; repeatable).
    #[arg(long = "env", value_name = "KEY=VAL")]
    pub env_vars: Vec<String>,
    /// Overwrite an existing profile if present.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ProfileInitArgs {
    /// Write a project-local `.agyva.toml` in the current directory.
    #[arg(long, conflicts_with = "global")]
    pub local: bool,
    /// Write a global named profile instead of `.agyva.toml`.
    #[arg(long, value_name = "NAME", conflicts_with = "local")]
    pub global: Option<String>,
    /// Mark the created global profile as the global default.
    #[arg(long, requires = "global")]
    pub default: bool,
    #[command(flatten)]
    pub fields: ProfileFieldsArgs,
}

#[derive(Debug, Args)]
pub struct ProfileCreateArgs {
    /// Global profile name.
    pub name: String,
    /// Mark this profile as the global default.
    #[arg(long)]
    pub default: bool,
    #[command(flatten)]
    pub fields: ProfileFieldsArgs,
}

#[derive(Debug, Args)]
pub struct ProfileSetArgs {
    /// Optional profile name followed by KEY and VALUE (`set [NAME] <KEY> <VALUE>`).
    #[arg(required = true, num_args = 2..=3, value_names = ["NAME_OR_KEY", "KEY_OR_VALUE", "VALUE"])]
    pub args: Vec<String>,
    /// Target the discovered local `.agyva.toml` explicitly.
    #[arg(long)]
    pub local: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StatuslineFormat {
    Compact,
    Full,
    Plain,
    Starship,
    Tmux,
    Waybar,
    Json,
}

#[derive(Debug, Args)]
pub struct StatuslineArgs {
    /// Output format (`compact`, `plain`, `starship`, `tmux`, `waybar`, `json`, `full`).
    #[arg(long, value_enum)]
    pub format: Option<StatuslineFormat>,
    /// Resolve thresholds and label from this profile.
    #[arg(long)]
    pub profile: Option<String>,
}

#[derive(Debug, Args)]
pub struct RouteArgs {
    #[command(subcommand)]
    pub command: Option<RouteCommand>,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum RouteCommand {
    /// Initialize `routing.toml` with a `default` group of saved accounts.
    Init {
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Add or update a named routing group in `routing.toml`.
    Add {
        group: String,
        /// Accounts in the group (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        accounts: Vec<String>,
        /// Ranking mode (`smart`, `priority`, or `least-used`).
        #[arg(long, value_enum, default_value_t = RouteRankingArg::Smart)]
        ranking: RouteRankingArg,
        /// Opt this group into auto-switching (still requires `auto_switch_ack = true`).
        #[arg(long)]
        auto_switch: bool,
    },
    /// Remove a named routing group from `routing.toml`.
    Remove {
        group: String,
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List ranked accounts and the reason for their position.
    List,
    /// Inspect cached ranking evidence.
    Inspect {
        group: String,
        /// Only accounts whose catalog offers this model family
        /// (exact family id or its effort-suffixed ids).
        #[arg(long)]
        for_model: Option<String>,
    },
    /// Recommend one account without changing anything.
    Plan {
        group: Option<String>,
        /// Only accounts whose catalog offers this model family
        /// (exact family id or its effort-suffixed ids).
        #[arg(long)]
        for_model: Option<String>,
    },
    /// Activate the group's recommended account (one journaled switch).
    Apply {
        group: String,
        /// Switch without asking for confirmation.
        #[arg(long)]
        yes: bool,
        /// Override a recommendation that no longer matches current evidence.
        #[arg(long)]
        force: bool,
    },
    /// Show recent routing decisions (bounded, no credentials).
    History {
        /// Maximum entries to show (1-50).
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RouteRankingArg {
    Smart,
    Priority,
    LeastUsed,
}

impl From<RouteRankingArg> for crate::core::route_config::Ranking {
    fn from(arg: RouteRankingArg) -> Self {
        match arg {
            RouteRankingArg::Smart => Self::Smart,
            RouteRankingArg::Priority => Self::Priority,
            RouteRankingArg::LeastUsed => Self::LeastUsed,
        }
    }
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Archive file to write (never overwritten; must be an absolute path).
    pub file: std::path::PathBuf,
    /// Export only this account (alias, number, email or id).
    #[arg(long)]
    pub account: Option<String>,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Alias for the account (default: derived from the email).
    #[arg(long)]
    pub alias: Option<String>,
    /// Update the saved credential if this account is already saved.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .multiple(false)
        .args(["selector", "account", "endpoint", "profile"])
))]
pub struct UseArgs {
    /// Alias, number, email, id (prefix), `ep:<name>`, or `-` for the previous account.
    #[arg(allow_hyphen_values = true)]
    pub selector: Option<String>,
    /// Switch to an OAuth account explicitly.
    #[arg(long)]
    pub account: Option<String>,
    /// Activate a custom endpoint explicitly.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Activate a named profile (switches its account and/or endpoint).
    #[arg(long)]
    pub profile: Option<String>,
    /// Replace an agy login that agyva has not saved.
    #[arg(long)]
    pub force: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RenameArgs {
    pub selector: String,
    pub new_alias: String,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    pub selector: String,
    /// Do not ask for confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Allow removing the active account.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct UsageArgs {
    /// Show only this account (alias, number, email or id).
    pub selector: Option<String>,
    /// Include every saved account (others are shown from the cache).
    #[arg(long)]
    pub all: bool,
    /// Compact single-line-per-account table.
    #[arg(long)]
    pub compact: bool,
    /// Apply account selection from a named profile.
    #[arg(long)]
    pub profile: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// Do not run agy; show cached data only.
    #[arg(long)]
    pub no_refresh: bool,
    /// Ignore the minimum refresh interval and also check a suspended account once.
    #[arg(long, conflicts_with = "no_refresh")]
    pub force_refresh: bool,
}

#[derive(Debug, Args)]
pub struct HealthArgs {
    /// Show only this account.
    pub selector: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Do not run agy; show cached data only.
    #[arg(long)]
    pub no_refresh: bool,
    /// Ignore the minimum refresh interval and also check a suspended account once.
    #[arg(long, conflicts_with = "no_refresh")]
    pub force_refresh: bool,
}

impl UsageArgs {
    pub fn mode(&self) -> crate::core::usage::RefreshMode {
        mode(self.no_refresh, self.force_refresh)
    }
}

impl HealthArgs {
    pub fn mode(&self) -> crate::core::usage::RefreshMode {
        mode(self.no_refresh, self.force_refresh)
    }
}

fn mode(no_refresh: bool, force: bool) -> crate::core::usage::RefreshMode {
    use crate::core::usage::RefreshMode;
    if force {
        RefreshMode::Force
    } else if no_refresh {
        RefreshMode::Skip
    } else {
        RefreshMode::Normal
    }
}

#[derive(Debug, Args)]
pub struct RecoverArgs {
    /// Undo the interrupted switch even if it could be completed.
    #[arg(long)]
    pub rollback: bool,
    /// Restore even if agy's current login is not part of the interrupted
    /// switch (an encrypted copy of it is kept first).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show all settings.
    Show,
    /// Change a setting.
    Set { key: String, value: String },
    /// Reset a setting back to its default value.
    Reset { key: String },
    /// Print the config file path.
    Path,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum VaultKeyModeArg {
    Auto,
    Keyring,
    Passphrase,
}

impl From<VaultKeyModeArg> for crate::config::VaultKeyMode {
    fn from(arg: VaultKeyModeArg) -> Self {
        match arg {
            VaultKeyModeArg::Auto => Self::Auto,
            VaultKeyModeArg::Keyring => Self::Keyring,
            VaultKeyModeArg::Passphrase => Self::Passphrase,
        }
    }
}

#[derive(Debug, Args)]
pub struct RekeyArgs {
    /// Target vault key mode (defaults to rotating the current mode).
    #[arg(long, value_enum)]
    pub mode: Option<VaultKeyModeArg>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellName {
    Fish,
    Bash,
    Zsh,
    Powershell,
    Elvish,
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Warn when remaining quota drops at or below PCT (%).
    #[arg(long, value_name = "PCT")]
    pub warn_below: Option<f64>,
    /// Alert critically when remaining quota drops at or below PCT (%).
    #[arg(long, value_name = "PCT")]
    pub critical_below: Option<f64>,
    /// Emit one JSON event object per line.
    #[arg(long)]
    pub json_stream: bool,
    /// Send desktop notifications on alerts (overrides config).
    #[arg(long)]
    pub notify: bool,
    /// Load threshold defaults from this profile.
    #[arg(long)]
    pub profile: Option<String>,
    /// Run a single watch cycle and exit.
    #[arg(long)]
    pub once: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AutoModel {
    Gemini,
    Claude,
}

#[derive(Debug, Args)]
pub struct AutoArgs {
    /// Model family to rotate (`gemini` or `claude`).
    #[arg(long, value_enum, ignore_case = true)]
    pub model: AutoModel,
    /// Evaluate once and exit. Implied by `--json`.
    #[arg(long)]
    pub once: bool,
    /// Print one JSON result and exit.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct MonitorArgs {
    /// Evaluation interval (cache-only), e.g. 15s, 30s, 1m.
    #[arg(long)]
    pub interval: Option<String>,
    /// Restrict ranking to this routing group.
    #[arg(long, conflicts_with = "all")]
    pub group: Option<String>,
    /// Evaluate all saved accounts (ignores any profile group).
    #[arg(long, conflicts_with = "group")]
    pub all: bool,
    /// Only accounts whose catalog offers this model family.
    #[arg(long)]
    pub for_model: Option<String>,
    /// Load defaults (group, interval, thresholds) from this profile.
    #[arg(long)]
    pub profile: Option<String>,
    /// Run a single monitor cycle and exit.
    #[arg(long)]
    pub once: bool,
    /// Print one MonitorSnapshot JSON document (implies --once).
    #[arg(long, conflicts_with = "json_stream")]
    pub json: bool,
    /// Emit NDJSON event/snapshot lines continuously.
    #[arg(long, conflicts_with = "json")]
    pub json_stream: bool,
    /// Warn when remaining quota drops at or below PCT (%).
    #[arg(long, value_name = "PCT")]
    pub warn_below: Option<f64>,
    /// Alert critically when remaining quota drops at or below PCT (%).
    #[arg(long, value_name = "PCT")]
    pub critical_below: Option<f64>,
}
