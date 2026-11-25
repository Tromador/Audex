/// The current Codex CLI version as embedded at compile time.
#[cfg(not(test))]
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tests render upstream's unreleased version, so snapshots do not depend on the
/// Audex release number compiled into production builds.
#[cfg(test)]
pub const CODEX_CLI_VERSION: &str = "0.0.0";
