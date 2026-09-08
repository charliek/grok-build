//! gx: where a tool result's images go on the Chat Completions wire.
//!
//! grok emits an image-bearing `ToolResult` as a `tool` message whose content
//! is `[Text, ImageUrl…]`. That is an xAI extension: the OpenAI Chat
//! Completions spec allows only text in a tool message. Live behaviour of the
//! providers gx ships presets for:
//!
//! * Meta's Muse gateway **400s** on it —
//!   `messages[N].content did not match any supported type` — but accepts the
//!   same image in a following `user` message.
//! * Z.AI (`glm-*`) and OpenRouter accept either shape.
//! * xAI accepts (and expects) the inline shape.
//!
//! So the default is derived, not configured: hoist for a non-xAI Chat
//! Completions endpoint, inline everywhere else. `[model.<id>].tool_result_images`
//! overrides it in either direction for a provider that disagrees with the
//! guess.
//!
//! The Responses and Messages backends have their own image encodings and are
//! untouched.
//!
//! One deliberate gap: [`crate::util::is_xai_api_url`] answers `true` for any
//! loopback host, because that is where xAI's cli-chat-proxy lives. A local
//! OpenAI-compatible server (Ollama, LM Studio) therefore keeps the inline
//! shape — which is exactly what it got before this existed, so nothing
//! regresses — and sets `tool_result_images = "hoist"` if its server rejects
//! that shape. Narrowing the predicate here would change the wire shape for
//! the proxy itself, which is a worse trade for a case no gx preset ships.

use serde::{Deserialize, Serialize};

use super::config::ModelInfo;
use xai_grok_sampling_types::ApiBackend;

/// Where a tool result's images are placed on the Chat Completions wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultImages {
    /// Image blocks inside the `tool` message (xAI's extension).
    Inline,
    /// A `user` message right after the tool message(s) that produced them.
    Hoist,
}

/// Whether this model's tool-result images are hoisted into a following user
/// message. An explicit `tool_result_images` always wins; otherwise every
/// non-xAI Chat Completions endpoint hoists.
pub(crate) fn hoist_tool_images(info: &ModelInfo) -> bool {
    match info.tool_result_images {
        Some(ToolResultImages::Inline) => false,
        Some(ToolResultImages::Hoist) => true,
        // An empty `base_url` means the endpoint is not known yet
        // (`ModelInfo::fallback` before `ModelEntry::fallback` fills it in), and
        // "not known" must not read as "not xAI" — that would silently change
        // xAI's own wire shape on an error path. Unknown stays inline.
        None => {
            info.api_backend == ApiBackend::ChatCompletions
                && !info.base_url.is_empty()
                && !crate::util::is_xai_api_url(&info.base_url)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(base_url: &str, api_backend: ApiBackend) -> ModelInfo {
        let mut info = ModelInfo::fallback("probe");
        info.base_url = base_url.to_owned();
        info.api_backend = api_backend;
        info
    }

    #[test]
    fn xai_chat_completions_stays_inline() {
        assert!(!hoist_tool_images(&info(
            "https://api.x.ai/v1",
            ApiBackend::ChatCompletions
        )));
    }

    #[test]
    fn third_party_chat_completions_hoists() {
        for base_url in [
            "https://api.z.ai/api/coding/paas/v4",
            "https://api.meta.ai/v1",
        ] {
            assert!(
                hoist_tool_images(&info(base_url, ApiBackend::ChatCompletions)),
                "{base_url} rejects (or may reject) images in a tool message"
            );
        }
    }

    /// Pins the module doc's deliberate gap: loopback reads as xAI's
    /// cli-chat-proxy, so a local OpenAI-compatible server keeps the inline
    /// shape (its pre-existing behaviour) unless it opts in explicitly.
    #[test]
    fn a_loopback_endpoint_stays_inline_until_it_opts_in() {
        let mut info = info("http://localhost:11434/v1", ApiBackend::ChatCompletions);
        assert!(!hoist_tool_images(&info));
        info.tool_result_images = Some(ToolResultImages::Hoist);
        assert!(hoist_tool_images(&info));
    }

    /// An unresolved endpoint must not read as "not xAI". `ModelInfo::fallback`
    /// leaves `base_url` empty until `ModelEntry::fallback` resolves it, and an
    /// error path that samples in between must keep xAI's inline shape.
    #[test]
    fn an_unresolved_base_url_stays_inline() {
        assert!(!hoist_tool_images(&info("", ApiBackend::ChatCompletions)));
    }

    #[test]
    fn responses_backend_is_untouched() {
        assert!(!hoist_tool_images(&info(
            "https://chatgpt.com/backend-api/codex",
            ApiBackend::Responses
        )));
    }

    #[test]
    fn explicit_inline_wins_on_a_third_party_endpoint() {
        let mut info = info("https://api.meta.ai/v1", ApiBackend::ChatCompletions);
        info.tool_result_images = Some(ToolResultImages::Inline);
        assert!(!hoist_tool_images(&info));
    }

    #[test]
    fn explicit_hoist_wins_on_xai() {
        let mut info = info("https://api.x.ai/v1", ApiBackend::ChatCompletions);
        info.tool_result_images = Some(ToolResultImages::Hoist);
        assert!(hoist_tool_images(&info));
    }

    #[test]
    fn the_config_spelling_is_lowercase() {
        assert_eq!(
            serde_json::to_value(ToolResultImages::Hoist).unwrap(),
            serde_json::json!("hoist")
        );
        assert_eq!(
            serde_json::from_value::<ToolResultImages>(serde_json::json!("inline")).unwrap(),
            ToolResultImages::Inline
        );
    }
}
