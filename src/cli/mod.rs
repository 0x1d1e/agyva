//! Command-line definition (clap) and dispatch.

mod commands;
pub mod monitor_cmd;
pub mod output;
pub mod usage_out;
mod watch_cmd;
use clap::{Args, Parser, Subcommand, ValueEnum};

pub use commands::{Io, run};

const EXIT_CODES: &str = "\
Examples:
  agyva add --alias personal       Save the current agy login
  agyva usage --all --no-refresh   Inspect cached quota without running agy
  agyva use work                   Switch manually, then restart agy
  agyva export /tmp/accounts.agy   Save every account to a portable archive
  agyva                            Open the terminal dashboard

Exit codes:
  0  success
  1  error (the message goes to stderr, followed by a `hint:` line)
  2  usage error (bad arguments, selector or setting)
  3  recovery required: an interrupted switch or import is pending, or a
     keyring write timed out and its outcome is unknown (run `agyva recover`)

Selectors (for use, rename, remove): an alias, a list number, an exact email,
an account id or unique id prefix, or `-` for the previous account.

Environment (development and tests): AGYVA_CONFIG_DIR, AGYVA_STATE_DIR,
AGYVA_CACHE_DIR, AGYVA_AGY_HOME, AGYVA_AGY_BIN, AGYVA_PASSPHRASE, AGYVA_LOG,
AGYVA_VAULT_KEY. AGYVA_BACKEND=memory|file (never touches the OS keyring)
needs AGYVA_CONFIG_DIR, AGYVA_STATE_DIR and AGYVA_AGY_HOME as well.
AGYVA_ARCHIVE_PASSPHRASE protects export/import archives (separate from the
vault passphrase). AGYVA_FAILPOINT works only with AGYVA_TEST_MODE=1.";

#[derive(Debug, Parser)]
#[command(
    name = "agyva",
    version,
    about = "Account manager for Google's Antigravity CLI (agy)",
    long_about = "Save agy logins in an encrypted vault and switch between them on request.\n\
                  Switching is manual unless a route group opts in to automation\n\
                  (auto_switch = true plus `config set auto_switch_ack true`).",
    after_help = EXIT_CODES,
    help_template = "{name} {version}\n{about-with-newline}\n{usage-heading} {usage}\n\nAccounts:\n  add          Save the current agy login\n  list         List saved accounts\n  use          Switch manually, then restart agy\n  current      Inspect agy's login\n  rename       Change an alias\n  remove       Forget a saved account\n\nQuota and health:\n  usage        Remaining quota by group and window\n  health       Account health and evidence\n  models       Cached model catalog (--refresh to update)\n  watch        Read-only foreground monitor\n  monitor      Route-aware cache-only monitor (--interval)\n\nDiagnostics and recovery:\n  status       State and pending recovery\n  doctor       Read-only checks\n  recover      Finish or undo an interrupted switch\n\nTransfer and updates:\n  export       Save accounts to a portable encrypted archive\n  import       Add accounts from an archive (skips existing)\n  update       Check GitHub for a newer release; install on request\n\nSetup:\n  config       View or change settings\n  completions  Fish, Bash or Zsh completion\n  version      agyva and agy versions\n  help         Help for a command\n\nOptions:\n{options}\n{after-help}",
    propagate_version = true
)]
pub struct Cli {
    /// Allow agy versions that agyva has not been verified against.
    #[arg(long, global = true)]
    pub allow_untested: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Save the account agy is currently logged in as.
    Add(AddArgs),
    /// List saved accounts.
    List(JsonFlag),
    /// Switch agy to a saved account (restart agy afterwards).
    Use(UseArgs),
    /// Show which account agy is logged in as.
    Current(JsonFlag),
    /// Change an account's alias.
    Rename(RenameArgs),
    /// Forget a saved account (agy's own login is never touched).
    Remove(RemoveArgs),
    /// Show quota usage (refreshes the active account; others come from the cache).
    Usage(UsageArgs),
    /// Rank saved accounts from cached quota without running agy.
    Route(RouteArgs),
    /// Monitor usage and health in the foreground (read-only; never switches).
    Watch,
    /// Route-aware monitor: cache-only ranking per interval, single-flight
    /// active refresh, never switches by itself.
    Monitor(MonitorArgs),
    /// Show per-account health (refreshes the active account).
    Health(HealthArgs),
    /// Show the cached model catalog (`agy models`); `--refresh` runs it now.
    Models(ModelsArgs),
    /// Summarize agyva's state and any pending recovery.
    Status(JsonFlag),
    /// Diagnose problems (read-only).
    Doctor(JsonFlag),
    /// Finish or undo an interrupted switch.
    Recover(RecoverArgs),
    /// View or change settings.
    #[command(subcommand)]
    Config(ConfigCommand),
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
    /// Check GitHub Releases for a newer agyva; install with --install.
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
/// With a group opted in (`auto_switch = true` + acknowledged
/// `auto_switch_ack`), `route apply` can be driven by the background
/// evaluation instead of a person; it still only ever switches through the
/// journaled path under the same gates.
pub struct RouteArgs {
    #[command(subcommand)]
    pub command: Option<RouteCommand>,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum RouteCommand {
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
pub struct UseArgs {
    /// Alias, number, email, id (prefix) or `-` for the previous account.
    #[arg(allow_hyphen_values = true)]
    pub selector: String,
    /// Replace an agy login that agyva has not saved.
    #[arg(long)]
    pub force: bool,
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
    /// Print the config file path.
    Path,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellName {
    Fish,
    Bash,
    Zsh,
}

#[derive(Debug, Args)]
pub struct MonitorArgs {
    /// Evaluation interval (cache-only), e.g. 15s, 30s, 1m.
    #[arg(long)]
    pub interval: Option<String>,
    /// Restrict ranking to this routing group.
    #[arg(long)]
    pub group: Option<String>,
    /// Only accounts whose catalog offers this model family.
    #[arg(long)]
    pub for_model: Option<String>,
}
