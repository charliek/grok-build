//! gx: pager façade for OpenAI (ChatGPT / Codex-plan) credentials.
//!
//! Mint, lock, and `gx providers token openai` live in
//! [`xai_grok_shell::auth::gx_openai_codex`]. This file re-exports the types
//! `providers_cmd` still calls and keeps [`run_login`] (PATH lookup of `codex`
//! is pager diagnostics).

pub(crate) use xai_grok_shell::auth::gx_openai_codex::{
    CodexPaths, Freshness, LOCK_TIMEOUT, account_check, codex_auth_json_path, freshness_from,
    read_auth_document, read_state, record_account, run_token,
};

/// `gx providers login openai`: a thin, honest wrapper around `codex login`.
/// gx does not implement its own OAuth flow; codex owns the credential store.
pub(crate) fn run_login() -> anyhow::Result<std::process::ExitStatus> {
    use anyhow::{Context as _, bail};

    let Some(codex) = crate::diagnostics::find_on_path("codex") else {
        bail!(
            "the `codex` CLI is not on PATH. gx signs in to OpenAI by delegating to it: \
             install codex (https://github.com/openai/codex), run `codex login`, then \
             re-run `gx providers status`."
        );
    };
    eprintln!("gx: running `{} login`", codex.display());
    std::process::Command::new(&codex)
        .arg("login")
        .status()
        .with_context(|| format!("failed to run {}", codex.display()))
}
