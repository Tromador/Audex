//! Build identity stays Audex even when managed-daemon staging uses bin/codex.
pub const COMMAND: &str = env!("CARGO_BIN_NAME");
pub const IS_AUDEX: bool = COMMAND.as_bytes()[0] == b'a';
pub const DISPLAY_NAME: &str = if IS_AUDEX { "audex" } else { "codex-cli" };
pub const USAGE: &str = concat!(
    env!("CARGO_BIN_NAME"),
    " [OPTIONS] [PROMPT]\n       ",
    env!("CARGO_BIN_NAME"),
    " [OPTIONS] <COMMAND> [ARGS]"
);
pub const EXEC_USAGE: &str = concat!(
    env!("CARGO_BIN_NAME"),
    " exec [OPTIONS] [PROMPT]\n       ",
    env!("CARGO_BIN_NAME"),
    " exec [OPTIONS] <COMMAND> [ARGS]"
);
pub const UPDATE_ABOUT: &str = if IS_AUDEX {
    "Show how to update Audex from a complete local package."
} else {
    "Update Codex to the latest version."
};
pub const UPDATE_GUIDANCE: &str = "Audex uses local source packages. Rebuild with the documented assemble-audex-package recipe, then explicitly select the package with `audex app-server daemon update --from-cli`. Upstream Codex updates are disabled for Audex.";

/// Called only at initial single-threaded process entry, before arg0 loads home state.
pub fn initialise() -> anyhow::Result<()> {
    if IS_AUDEX {
        codex_install_context::initialise_audex_product();
        if std::env::var_os("CODEX_HOME").is_none() {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("HOME is required for the Audex runtime home"))?;
            let runtime_home = std::path::PathBuf::from(home).join(".chatgpt/Audex/codex-home");
            std::fs::create_dir_all(&runtime_home)?;
            // SAFETY: main calls this before creating threads or initialising libraries.
            unsafe {
                std::env::set_var("CODEX_HOME", runtime_home);
            }
        }
    }
    Ok(())
}
