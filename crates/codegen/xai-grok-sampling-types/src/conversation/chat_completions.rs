//! Chat Completions wire format.

use super::*;

impl From<ChatRequestMessage> for ConversationItem {
    fn from(msg: ChatRequestMessage) -> Self {
        match msg.role {
            Role::System => ConversationItem::System(SystemItem {
                content: Arc::<str>::from(msg.text_content()),
            }),
            Role::User => {
                let parts = msg
                    .content
                    .blocks()
                    .into_iter()
                    .map(|block| match block {
                        ChatContentBlock::Text { text } => ContentPart::Text {
                            text: Arc::<str>::from(text),
                        },
                        ChatContentBlock::ImageUrl { image_url } => ContentPart::Image {
                            url: Arc::<str>::from(image_url.url),
                        },
                    })
                    .collect();
                ConversationItem::User(UserItem {
                    content: parts,
                    synthetic_reason: None,
                    ..Default::default()
                })
            }
            Role::Assistant => {
                // Reasoning is a sibling item, which a single-item conversion
                // cannot emit, so it is dropped here.
                let content = msg.text_content();
                let model_id = msg.model_id;

                let tool_calls: Vec<ToolCall> = msg
                    .tool_calls
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: Arc::<str>::from(tc.id.unwrap_or_default()),
                        name: tc.function.name,
                        arguments: Arc::<str>::from(tc.function.arguments),
                    })
                    .collect();

                ConversationItem::Assistant(AssistantItem {
                    content: Arc::<str>::from(content),
                    tool_calls,
                    model_id,
                    model_fingerprint: None,
                    reasoning_effort: None,
                })
            }
            Role::Tool => {
                let content = msg.text_content();
                ConversationItem::ToolResult(ToolResultItem {
                    tool_call_id: msg.tool_call_id.unwrap_or_default(),
                    content: Arc::<str>::from(content),
                    images: Vec::new(),
                })
            }
        }
    }
}

/// Convert a single non-`Reasoning` [`ConversationItem`]. The wire format
/// carries `reasoning_content` on the *following* assistant message, which a
/// single item cannot see, so use [`conversation_to_chat_messages`] instead
/// when reasoning must survive.
pub fn conversation_item_to_chat_message(item: ConversationItem) -> ChatRequestMessage {
    match item {
        ConversationItem::System(s) => ChatRequestMessage::system(s.content.as_ref()),
        ConversationItem::User(u) => {
            let has_images = u
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::Image { .. }));
            // Collapse to a single text block when there are no images, as
            // the pre-blocks behavior did.
            let content = if !has_images {
                let text = u
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                MessageContent::Text(text)
            } else {
                let blocks: Vec<ChatContentBlock> = u
                    .content
                    .into_iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => ChatContentBlock::Text {
                            text: text.as_ref().to_owned(),
                        },
                        ContentPart::Image { url } => ChatContentBlock::ImageUrl {
                            image_url: ImageUrl {
                                url: url.as_ref().to_owned(),
                            },
                        },
                    })
                    .collect();
                MessageContent::Blocks(blocks)
            };
            ChatRequestMessage {
                role: Role::User,
                content,
                name: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                model_id: None,
                reasoning_content: None,
            }
        }
        ConversationItem::Assistant(a) => {
            let tool_calls: Vec<ToolCallRequest> = a
                .tool_calls
                .into_iter()
                .map(|tc| {
                    let arguments = sanitize_tool_arguments(&tc.id, &tc.name, tc.arguments.clone());
                    ToolCallRequest::function(tc.name, arguments.as_ref().to_owned())
                        .with_id(tc.id.as_ref().to_owned())
                })
                .collect();

            ChatRequestMessage {
                role: Role::Assistant,
                content: MessageContent::Text(a.content.as_ref().to_owned()),
                name: None,
                tool_calls,
                tool_call_id: None,
                model_id: a.model_id,
                reasoning_content: None,
            }
        }
        ConversationItem::ToolResult(t) => {
            if t.images.is_empty() {
                ChatRequestMessage::tool(t.tool_call_id, t.content.as_ref().to_owned())
            } else {
                let mut blocks = vec![ChatContentBlock::Text {
                    text: t.content.as_ref().to_owned(),
                }];
                for img in t.images {
                    if let ContentPart::Image { url } = img {
                        blocks.push(ChatContentBlock::ImageUrl {
                            image_url: ImageUrl {
                                url: url.as_ref().to_owned(),
                            },
                        });
                    }
                }
                ChatRequestMessage {
                    role: Role::Tool,
                    content: MessageContent::Blocks(blocks),
                    name: None,
                    tool_calls: Vec::new(),
                    tool_call_id: Some(t.tool_call_id),
                    model_id: None,
                    reasoning_content: None,
                }
            }
        }
        // Backend tool calls have no Chat Completions equivalent.
        // Emit a synthetic assistant message so the model sees context
        // about what was searched, without breaking the message sequence.
        ConversationItem::BackendToolCall(b) => ChatRequestMessage {
            role: Role::Assistant,
            content: MessageContent::Text(b.text_summary()),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            model_id: None,
            reasoning_content: None,
        },
        // The only caller folds `Reasoning` into the following assistant.
        ConversationItem::Reasoning(_) => unreachable!(
            "conversation_to_chat_messages folds Reasoning siblings; \
                 conversation_item_to_chat_message is never called with one"
        ),
    }
}

/// The canonical conversion. Each run of `Reasoning` siblings folds into the
/// `reasoning_content` of the following `Assistant`; a `BackendToolCall` in
/// between does not break the fold, any other item clears it, and reasoning
/// with no following assistant is dropped.
///
/// This is the *wire* conversion: every non-test caller feeds the result
/// straight into a [`ChatCompletionRequest`], so `model_id` is cleared here
/// (see [`strip_wire_incompatible_message_fields`]). Callers that need
/// `model_id` preserved must use [`conversation_item_to_chat_message`], which
/// is the on-disk / v0-downgrade conversion and is left untouched.
pub fn conversation_to_chat_messages(items: Vec<ConversationItem>) -> Vec<ChatRequestMessage> {
    let mut out: Vec<ChatRequestMessage> = Vec::with_capacity(items.len());
    let mut pending_reasoning: Vec<String> = Vec::new();

    for item in items {
        match item {
            ConversationItem::Reasoning(r) => {
                let text = reasoning_item_text(&r);
                if !text.is_empty() {
                    pending_reasoning.push(text);
                }
            }
            ConversationItem::Assistant(_) => {
                let mut msg = conversation_item_to_chat_message(item);
                if !pending_reasoning.is_empty() {
                    msg.reasoning_content = Some(pending_reasoning.join("\n"));
                    pending_reasoning.clear();
                }
                out.push(msg);
            }
            ConversationItem::BackendToolCall(_) => {
                // Keep `pending_reasoning` so it still folds onto the
                // following assistant, as the Responses path does.
                out.push(conversation_item_to_chat_message(item));
            }
            other => {
                pending_reasoning.clear();
                out.push(conversation_item_to_chat_message(other));
            }
        }
    }

    strip_wire_incompatible_message_fields(&mut out);
    out
}

/// Drop per-message fields that OpenAI-compatible hosts reject.
///
/// `messages[n].model_id` is an xAI-only extension. Fireworks and Groq
/// 400 on the unknown key (GLM gateways tolerate it), and no server
/// consumes it, so it is cleared on the way to the wire only — the field
/// stays on [`ChatRequestMessage`] for the v0 on-disk format.
fn strip_wire_incompatible_message_fields(messages: &mut [ChatRequestMessage]) {
    for msg in messages {
        msg.model_id = None;
    }
}

/// OpenAI-compatible hosts (Fireworks, Groq, some GLM gateways) reject
/// JSON Schema fragments that include `"default": null`. Schemars emits that
/// for `Option<T>` + `#[serde(default)]`. Stripping null defaults keeps the
/// schema valid without changing required-field semantics.
///
/// Applied to every Chat Completions request, first-party xAI included: the
/// key carries no meaning any server reads (an absent `default` and a
/// `default: null` are equivalent for a nullable field), so removing it
/// unconditionally keeps one wire shape instead of a per-provider fork.
///
/// `const`, `enum`, `examples`, and a non-null `default` hold arbitrary
/// *instance* data, not nested schema (e.g. `{"const": {"default": null}}`
/// is a literal object, not a schema fragment with a stray null default) —
/// MCP tools provide runtime-generated schemas where this matters. Their
/// values are kept byte-for-byte, with no recursion into them.
///
/// `pub` (re-exported via [`crate::conversation`]) because the compaction
/// request builder in `xai-grok-shell` constructs a `ChatCompletionRequest`
/// directly rather than going through `From<ConversationRequest>`, so it
/// must apply this sanitizer to its own tool schemas.
pub fn sanitize_json_schema_for_compat(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let cleaned = map
                .into_iter()
                .filter_map(|(k, v)| match k.as_str() {
                    "default" if v.is_null() => None,
                    "default" | "const" | "enum" | "examples" => Some((k, v)),
                    _ => Some((k, sanitize_json_schema_for_compat(v))),
                })
                .collect();
            serde_json::Value::Object(cleaned)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.into_iter()
                .map(sanitize_json_schema_for_compat)
                .collect(),
        ),
        other => other,
    }
}

impl From<ChatResponseMessage> for ConversationItem {
    fn from(msg: ChatResponseMessage) -> Self {
        // Reasoning is dropped: the streaming consumer synthesizes the
        // sibling item instead.
        let content = msg.content.unwrap_or_default();

        let tool_calls: Vec<ToolCall> = msg
            .tool_calls
            .into_iter()
            .map(|tc| ToolCall {
                id: Arc::<str>::from(tc.id),
                name: tc.function.name,
                arguments: Arc::<str>::from(tc.function.arguments),
            })
            .collect();

        ConversationItem::Assistant(AssistantItem {
            content: Arc::<str>::from(content),
            tool_calls,
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }
}

impl From<ConversationRequest> for ChatCompletionRequest {
    fn from(req: ConversationRequest) -> Self {
        let messages: Vec<ChatRequestMessage> = conversation_to_chat_messages(req.items);

        let tools_is_empty = req.tools.is_empty();
        let tools: Option<Vec<ToolDefinition>> = if tools_is_empty {
            None
        } else {
            Some(
                req.tools
                    .into_iter()
                    .map(|t| {
                        ToolDefinition::function(
                            t.name,
                            t.description,
                            sanitize_json_schema_for_compat(t.parameters),
                        )
                    })
                    .collect(),
            )
        };

        // only set `tool_choice` when there are `tools` to avoid OpenAI client errors
        let tool_choice = req
            .tool_choice
            .filter(|_| !tools_is_empty)
            .map(|tc| match tc {
                ConversationToolChoice::Auto => ToolChoice::auto(),
                ConversationToolChoice::None => ToolChoice::none(),
                ConversationToolChoice::Required => ToolChoice::required(),
                ConversationToolChoice::Function(name) => ToolChoice::function(name),
            });

        let response_format = req
            .json_schema
            .map(|schema| rs::ResponseFormat::JsonSchema {
                json_schema: rs::ResponseFormatJsonSchema {
                    description: None,
                    name: STRUCTURED_OUTPUT_SCHEMA_NAME.to_string(),
                    schema: Some(schema),
                    strict: Some(true),
                },
            });

        ChatCompletionRequest {
            model: req.model,
            messages,
            temperature: req.temperature,
            max_tokens: req.max_output_tokens,
            top_p: req.top_p,
            frequency_penalty: None,
            presence_penalty: None,
            user: None,
            tools,
            tool_choice,
            search_parameters: None,
            response_format,
            reasoning_effort: req.reasoning_effort,
            x_grok_conv_id: req.x_grok_conv_id,
            x_grok_req_id: req.x_grok_req_id,
            x_grok_session_id: req.x_grok_session_id,
            x_grok_turn_idx: req.x_grok_turn_idx,
            x_grok_agent_id: req.x_grok_agent_id,
            x_grok_deployment_id: req.x_grok_deployment_id,
            x_grok_user_id: req.x_grok_user_id,
            trace: None,
        }
    }
}

/// OpenAI-compat hardening: `messages[n].model_id` must never reach the
/// wire, and tool schemas must not carry `"default": null`. Both apply to
/// first-party xAI requests too, so the xAI-shaped regression below pins
/// that nothing *else* about the request changed.
#[cfg(test)]
mod compat_tests {
    use super::*;

    /// Recursively collect the JSON pointers at which `key` appears.
    fn find_key_paths(value: &serde_json::Value, key: &str, path: &str, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    let child = format!("{path}/{k}");
                    if k == key {
                        out.push(child.clone());
                    }
                    find_key_paths(v, key, &child, out);
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    find_key_paths(v, key, &format!("{path}/{i}"), out);
                }
            }
            _ => {}
        }
    }

    fn null_default_paths(value: &serde_json::Value) -> Vec<String> {
        let mut all = Vec::new();
        find_key_paths(value, "default", "", &mut all);
        all.into_iter()
            .filter(|p| {
                value
                    .pointer(p)
                    .map(serde_json::Value::is_null)
                    .unwrap_or(false)
            })
            .collect()
    }

    fn reasoning_sibling(id: &str, text: &str) -> ConversationItem {
        ConversationItem::Reasoning(rs::ReasoningItem {
            id: id.to_string(),
            summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text: text.to_string(),
            })],
            content: None,
            encrypted_content: None,
            status: None,
        })
    }

    /// Tool schema with the schemars artifact (`"default": null` from
    /// `Option<T>` + `#[serde(default)]`) plus real defaults that must survive.
    fn tool_with_null_default() -> ToolSpec {
        ToolSpec {
            name: "run_script".to_string(),
            description: Some("Run a script".to_string()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "args": {
                        "description": "JSON value bound to the script's `args` global.",
                        "default": null
                    },
                    "name": { "type": "string", "default": "ok" },
                    "nested": {
                        "type": "object",
                        "properties": {
                            "deep": { "type": "string", "default": null }
                        }
                    },
                    "variants": [
                        { "type": "string", "default": null },
                        { "type": "number", "default": 0 }
                    ]
                },
                "required": ["name"]
            }),
        }
    }

    #[test]
    fn chat_request_message_omits_model_id_on_the_wire() {
        let req = ConversationRequest::from_items(vec![
            ConversationItem::user("ping"),
            ConversationItem::Assistant(AssistantItem {
                content: "pong".into(),
                tool_calls: vec![],
                model_id: Some("Qwen 3.8 Max".to_string()),
                model_fingerprint: None,
                reasoning_effort: None,
            }),
        ]);

        let wire = serde_json::to_value(ChatCompletionRequest::from(req)).unwrap();
        let mut found = Vec::new();
        find_key_paths(&wire, "model_id", "", &mut found);
        assert!(
            found.is_empty(),
            "OpenAI-compat hosts 400 on messages[].model_id, found at {found:?} in {wire}"
        );
        assert_eq!(wire["messages"][1]["content"], "pong");
    }

    /// `conversation_to_chat_messages` is the shared wire converter — the
    /// `From<ConversationRequest>` impl *and* the compaction request builder
    /// in `xai-grok-shell` both go through it, so the strip lives here.
    #[test]
    fn conversation_to_chat_messages_clears_model_id() {
        let msgs =
            conversation_to_chat_messages(vec![ConversationItem::Assistant(AssistantItem {
                content: "hi".into(),
                tool_calls: vec![],
                model_id: Some("grok-4".to_string()),
                model_fingerprint: None,
                reasoning_effort: None,
            })]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].model_id, None);

        // A directly-built ChatCompletionRequest (the compaction shape)
        // therefore also serializes without it.
        let wire =
            serde_json::to_value(ChatCompletionRequest::new("some-compat-model", msgs)).unwrap();
        let mut found = Vec::new();
        find_key_paths(&wire, "model_id", "", &mut found);
        assert!(
            found.is_empty(),
            "compaction request leaked model_id: {wire}"
        );
    }

    /// The v0 on-disk downgrade path (`chat-history-downgrade`) uses the
    /// single-item converter and must keep carrying `model_id`.
    #[test]
    fn single_item_conversion_preserves_model_id_for_v0_downgrade() {
        let msg = conversation_item_to_chat_message(ConversationItem::Assistant(AssistantItem {
            content: "hi".into(),
            tool_calls: vec![],
            model_id: Some("grok-4".to_string()),
            model_fingerprint: None,
            reasoning_effort: None,
        }));
        assert_eq!(msg.model_id.as_deref(), Some("grok-4"));
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v["model_id"], "grok-4",
            "v0 JSONL lines still record model_id"
        );
    }

    #[test]
    fn strips_null_defaults_nested() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "args": {
                    "description": "JSON value bound to the script's `args` global.",
                    "default": null
                },
                "name": { "type": "string", "default": "ok" }
            }
        });
        let out = sanitize_json_schema_for_compat(input);
        assert!(out["properties"]["args"].get("default").is_none());
        assert_eq!(out["properties"]["name"]["default"], "ok");
    }

    #[test]
    fn tool_schemas_reach_the_wire_without_null_defaults() {
        let req = ConversationRequest::from_items(vec![ConversationItem::user("go")])
            .with_tools(vec![tool_with_null_default()]);

        let wire = serde_json::to_value(ChatCompletionRequest::from(req)).unwrap();
        let tools = &wire["tools"];
        assert_eq!(null_default_paths(tools), Vec::<String>::new(), "{tools}");

        let params = &tools[0]["function"]["parameters"];
        // Only the null defaults go; everything else is byte-identical.
        assert!(params["properties"]["args"].get("default").is_none());
        assert_eq!(
            params["properties"]["args"]["description"],
            "JSON value bound to the script's `args` global."
        );
        assert_eq!(params["properties"]["name"]["default"], "ok");
        assert!(
            params["properties"]["nested"]["properties"]["deep"]
                .get("default")
                .is_none()
        );
        assert!(params["properties"]["variants"][0].get("default").is_none());
        assert_eq!(params["properties"]["variants"][1]["default"], 0);
        assert_eq!(params["required"], serde_json::json!(["name"]));
    }

    /// First-party xAI regression: the sanitizer and the `model_id` strip run
    /// for xAI requests too (deliberate — one wire shape for all providers).
    /// A multi-turn request with assistant `model_id` + reasoning and a tool
    /// carrying defaults must lose *only* `model_id` and `"default": null`.
    #[test]
    fn xai_shaped_request_is_otherwise_unchanged() {
        let items = vec![
            ConversationItem::system("You are helpful."),
            ConversationItem::user("fix the bug"),
            reasoning_sibling("rs_1", "let me read the file"),
            ConversationItem::Assistant(AssistantItem {
                content: String::new().into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "run_script".to_string(),
                    arguments: r#"{"name":"build"}"#.into(),
                }],
                model_id: Some("grok-4".to_string()),
                model_fingerprint: Some("fp_abc".to_string()),
                reasoning_effort: None,
            }),
            ConversationItem::tool_result("call_1", "exit 0"),
            reasoning_sibling("rs_2", "the build is green"),
            ConversationItem::Assistant(AssistantItem {
                content: "all done".into(),
                tool_calls: vec![],
                model_id: Some("grok-4".to_string()),
                model_fingerprint: None,
                reasoning_effort: None,
            }),
        ];
        let req = ConversationRequest::from_items(items)
            .with_model("grok-4")
            .with_tools(vec![tool_with_null_default()]);

        let wire = serde_json::to_value(ChatCompletionRequest::from(req)).unwrap();

        // Nothing removed but the two compat artifacts.
        let mut model_ids = Vec::new();
        find_key_paths(&wire, "model_id", "", &mut model_ids);
        assert!(model_ids.is_empty(), "{wire}");
        assert_eq!(null_default_paths(&wire), Vec::<String>::new(), "{wire}");

        // Top-level request fields untouched.
        assert_eq!(wire["model"], "grok-4");
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 5, "{wire}");

        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are helpful.");
        assert_eq!(messages[1]["role"], "user");

        // Assistant tool-call turn: reasoning folded, tool_calls intact.
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["reasoning_content"], "let me read the file");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["name"],
            "run_script"
        );
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"name":"build"}"#
        );

        // Tool result and final assistant turn.
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "exit 0");
        assert_eq!(messages[4]["role"], "assistant");
        assert_eq!(messages[4]["content"], "all done");
        assert_eq!(messages[4]["reasoning_content"], "the build is green");

        // Tool definition still complete.
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["function"]["name"], "run_script");
        assert_eq!(wire["tools"][0]["function"]["description"], "Run a script");
        assert_eq!(
            wire["tools"][0]["function"]["parameters"]["properties"]["name"]["default"],
            "ok"
        );
    }

    /// `const` holds an instance-data literal, not a nested schema: a
    /// `default: null` inside it must survive verbatim, not be stripped.
    #[test]
    fn const_value_with_nested_default_null_is_preserved_verbatim() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {
                    "const": { "default": null }
                }
            }
        });
        let out = sanitize_json_schema_for_compat(input.clone());
        assert_eq!(out, input);
    }

    /// `enum` is a literal array of instance values; objects inside it
    /// (including their own `default: null`) must not be touched.
    #[test]
    fn enum_array_with_object_default_null_is_preserved_verbatim() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "choice": {
                    "enum": [
                        { "default": null },
                        { "a": 1, "default": null }
                    ]
                }
            }
        });
        let out = sanitize_json_schema_for_compat(input.clone());
        assert_eq!(out, input);
    }

    /// `examples` is sample instance data, not schema; preserved byte-for-byte.
    #[test]
    fn examples_value_is_preserved_verbatim() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "args": {
                    "examples": [{ "default": null }, null, { "a": { "default": null } }]
                }
            }
        });
        let out = sanitize_json_schema_for_compat(input.clone());
        assert_eq!(out, input);
    }

    /// A non-null object-valued `default` is itself instance data (the
    /// literal default value), so it must be kept verbatim with no
    /// recursion — even though it happens to contain a `default: null` key
    /// of its own.
    #[test]
    fn non_null_object_default_is_preserved_verbatim() {
        let input = serde_json::json!({
            "default": { "a": null, "default": null }
        });
        let out = sanitize_json_schema_for_compat(input.clone());
        assert_eq!(out, input);
    }

    /// Existing behavior is unchanged: a schema-position `default: null`
    /// (not under `const`/`enum`/`examples`) is still stripped, whether at
    /// the top level or nested under `properties`/`items`.
    #[test]
    fn schema_position_null_defaults_still_stripped() {
        let input = serde_json::json!({
            "default": null,
            "type": "object",
            "properties": {
                "args": { "default": null },
                "items": {
                    "type": "array",
                    "items": { "type": "string", "default": null }
                }
            }
        });
        let out = sanitize_json_schema_for_compat(input);
        assert!(out.get("default").is_none());
        assert!(out["properties"]["args"].get("default").is_none());
        assert!(out["properties"]["items"]["items"].get("default").is_none());
    }
}
