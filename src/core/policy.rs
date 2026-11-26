//! The one-time policy notice and the standing restart warning.

pub const POLICY_NOTICE: &str = "\
Policy: manual `agyva use` is still the normal switch. `agyva auto` is the
explicit rotator for one model family when usage is over 90%. It does not
bypass suspensions or usage limits. A running agy keeps the login it already
loaded. You remain responsible for following Google's terms for every account
you save. See docs/security.md.";

pub const RESTART_NOTICE: &str = "Restart agy to use the new account; agy sessions that are already running keep their old login.";
