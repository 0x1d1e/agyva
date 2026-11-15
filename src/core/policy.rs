//! The one-time policy notice and the standing restart warning.

pub const POLICY_NOTICE: &str = "\
Policy: agyva switches accounts only when you ask it to (`agyva use`).
It never rotates, balances or auto-switches accounts, and it is not a way to
work around usage limits. You remain responsible for following Google's terms
for every account you save. See docs/security.md.";

pub const RESTART_NOTICE: &str = "Restart agy to use the new account; agy sessions that are already running keep their old login.";
