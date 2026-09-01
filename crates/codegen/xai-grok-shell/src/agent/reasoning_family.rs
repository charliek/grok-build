//! gx: which backend minted sealed reasoning for a catalog model.
//!
//! Upstream family-switch compact only fires when both models have a
//! `model_family` and they differ. gx ChatGPT/Codex presets historically
//! left `model_family` unset, so Sol → Grok never compacted and Grok 400'd
//! on OpenAI `encrypted_content`. This helper infers `"openai-codex"` from
//! `codex_compat` **and** the canonical ChatGPT Codex URL so existing
//! installs work without re-running `gx providers install`. Explicit
//! `model_family` always wins. Chat Completions models stay `None` so
//! GLM ↔ Kimi does not compact.
//!
//! Compact using this family is **user-initiated switches only** — see
//! `enable_family_compact` on `handlers::model_switch::apply`. Resume/load
//! must not treat spawn-default Grok vs persisted Sol as a family switch.

use super::config::ModelInfo;

const OPENAI_CODEX_FAMILY: &str = "openai-codex";
const CHATGPT_CODEX_URL_MARK: &str = "chatgpt.com/backend-api/codex";

pub(crate) fn reasoning_family(info: &ModelInfo) -> Option<&str> {
    if let Some(family) = info.model_family.as_deref() {
        return Some(family);
    }
    if info.codex_compat == Some(true) && is_chatgpt_codex_endpoint(&info.base_url) {
        return Some(OPENAI_CODEX_FAMILY);
    }
    None
}

pub(crate) fn families_differ(old: Option<&str>, new: Option<&str>) -> bool {
    matches!((old, new), (Some(a), Some(b)) if a != b)
}

fn is_chatgpt_codex_endpoint(base_url: &str) -> bool {
    base_url.contains(CHATGPT_CODEX_URL_MARK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::ModelInfo;

    fn info() -> ModelInfo {
        ModelInfo::fallback("probe")
    }

    #[test]
    fn explicit_model_family_wins_over_codex_compat() {
        let mut info = info();
        info.model_family = Some("custom".into());
        info.codex_compat = Some(true);
        info.base_url = "https://chatgpt.com/backend-api/codex".into();
        assert_eq!(reasoning_family(&info), Some("custom"));
    }

    #[test]
    fn sol_shaped_preset_infers_openai_codex() {
        let mut info = info();
        info.codex_compat = Some(true);
        info.base_url = "https://chatgpt.com/backend-api/codex".into();
        assert_eq!(reasoning_family(&info), Some("openai-codex"));
    }

    #[test]
    fn codex_compat_on_public_openai_api_does_not_infer() {
        let mut info = info();
        info.codex_compat = Some(true);
        info.base_url = "https://api.openai.com/v1".into();
        assert_eq!(reasoning_family(&info), None);
    }

    #[test]
    fn grok_catalog_family_is_xai() {
        let mut info = info();
        info.model_family = Some("xai".into());
        assert_eq!(reasoning_family(&info), Some("xai"));
    }

    #[test]
    fn glm_shaped_chat_completions_stays_none() {
        let mut info = info();
        info.base_url = "https://api.z.ai/api/coding/paas/v4".into();
        assert_eq!(reasoning_family(&info), None);
    }

    #[test]
    fn sol_and_grok_differ() {
        assert!(families_differ(Some("openai-codex"), Some("xai")));
        assert!(!families_differ(Some("openai-codex"), Some("openai-codex")));
        assert!(!families_differ(None, Some("xai")));
        assert!(!families_differ(None, None));
    }
}
