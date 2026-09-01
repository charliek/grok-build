use super::*;

/// Flatten `response.output` into `ConversationItem`s, preserving emission order.
/// Replaying that order byte for byte on the next turn is what keeps the server-side prefix cache hot.
pub fn response_to_conversation_items(response: rs::Response) -> Vec<ConversationItem> {
    let model_id = response.model.clone();
    let model_fingerprint = response
        .metadata
        .as_ref()
        .and_then(|m| m.get("system_fingerprint"))
        .cloned()
        .filter(|s| !s.is_empty());
    let reasoning_effort = response
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .map(crate::ReasoningEffort::from_responses_api);

    let mut items: Vec<ConversationItem> = Vec::with_capacity(response.output.len() + 1);
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut backend_tool_count: usize = 0;

    for item in response.output {
        match item {
            rs::OutputItem::Message(msg) => {
                for content_part in msg.content {
                    if let rs::OutputMessageContent::OutputText(text_content) = content_part {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&text_content.text);
                    }
                }
            }
            rs::OutputItem::FunctionCall(fc) => {
                // Tied to the assistant turn: a ToolResult must follow each one in conversation order, so they are not siblings
                tool_calls.push(ToolCall {
                    id: Arc::<str>::from(fc.call_id),
                    name: fc.name,
                    arguments: Arc::<str>::from(fc.arguments),
                });
            }
            rs::OutputItem::Reasoning(r) => {
                items.push(ConversationItem::Reasoning(r));
            }
            // These calls already ran server-side; they are kept so later turns replay the same context
            rs::OutputItem::WebSearchCall(ws) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::WebSearch(ws),
                }));
            }
            rs::OutputItem::CustomToolCall(ct) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::XSearch(ct),
                }));
            }
            rs::OutputItem::CodeInterpreterCall(ci) => {
                backend_tool_count += 1;
                items.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                    kind: BackendToolKind::CodeInterpreter(ci),
                }));
            }
            rs::OutputItem::McpCall(_) => {
                backend_tool_count += 1;
            }
            _ => {}
        }
    }

    if backend_tool_count > 0 {
        tracing::info!(
            backend_tool_count,
            "response contained backend-executed tool calls"
        );
    }

    tracing::info!(model_id = %model_id, ?model_fingerprint, ?reasoning_effort, "response_to_conversation_items setting model metadata on AssistantItem");
    items.push(ConversationItem::Assistant(AssistantItem {
        content: Arc::<str>::from(content),
        tool_calls,
        model_id: Some(model_id),
        model_fingerprint,
        reasoning_effort,
    }));

    items
}

impl From<&ConversationRequest> for rs::CreateResponse {
    fn from(req: &ConversationRequest) -> Self {
        let input = build_responses_input(req);
        let tools = build_responses_tools(req);

        let tool_choice = req.tool_choice.as_ref().map(|tc| match tc {
            ConversationToolChoice::Auto => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Auto),
            ConversationToolChoice::None => rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::None),
            ConversationToolChoice::Required => {
                rs::ToolChoiceParam::Mode(rs::ToolChoiceOptions::Required)
            }
            ConversationToolChoice::Function(name) => {
                rs::ToolChoiceParam::Function(rs::ToolChoiceFunction { name: name.clone() })
            }
        });

        let text = req
            .json_schema
            .as_ref()
            .map(|schema| rs::ResponseTextParam {
                format: rs::TextResponseFormatConfiguration::JsonSchema(
                    rs::ResponseFormatJsonSchema {
                        description: None,
                        name: STRUCTURED_OUTPUT_SCHEMA_NAME.to_string(),
                        schema: Some(schema.clone()),
                        strict: Some(true),
                    },
                ),
                verbosity: None,
            });

        // gx: OpenAI's ChatGPT/Codex endpoint validates this body far more
        // strictly than the public Responses API (spike 0A.1, verified live):
        //
        // - `store` must be present and `false` — omitting it is a 400
        //   ("Store must be set to false"), and grok's mapping otherwise emits
        //   `None`.
        // - `temperature`, `top_p`, and `max_output_tokens` are each a 400
        //   ("Unsupported parameter"), whatever their value — so they are
        //   dropped here rather than merely left unset, which is why the
        //   suppression lives in the mapping and not at the call sites.
        // - `include: ["reasoning.encrypted_content"]` is what makes multi-turn
        //   reasoning replay work with `store: false`.
        //
        // `instructions` stays `None`: grok sends its system prompt as an input
        // item, and the endpoint accepts a body without `instructions`.
        let codex = req.codex_compat;
        let include = codex.then(|| vec![rs::IncludeEnum::ReasoningEncryptedContent]);

        rs::CreateResponse {
            background: None,
            conversation: None,
            include,
            input,
            instructions: None,
            max_output_tokens: if codex { None } else { req.max_output_tokens },
            max_tool_calls: None,
            metadata: None,
            model: req.model.clone(),
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: req
                .prompt_cache_key
                .clone()
                .or_else(|| req.x_grok_conv_id.clone()),
            prompt_cache_retention: None,
            reasoning: Some(rs::Reasoning {
                effort: req.reasoning_effort.map(|e| e.to_responses_api()),
                summary: Some(rs::ReasoningSummary::Concise),
            }),
            safety_identifier: None,
            service_tier: None,
            store: codex.then_some(false),
            stream: None,
            stream_options: None,
            temperature: if codex { None } else { req.temperature },
            text,
            tool_choice,
            tools: if tools.is_empty() { None } else { Some(tools) },
            top_logprobs: None,
            top_p: if codex { None } else { req.top_p },
            truncation: None,
        }
    }
}

/// Reasoning items stay top-level siblings rather than folding into the assistant, so the input replays the model's original order.
pub(super) fn build_responses_input(req: &ConversationRequest) -> rs::InputParam {
    let mut items: Vec<rs::InputItem> = if req.codex_compat {
        req.items
            .iter()
            .enumerate()
            .filter(|(i, item)| match item {
                ConversationItem::Reasoning(r) => keep_codex_reasoning(
                    r,
                    following_assistant_model_id(&req.items, *i),
                    req.model.as_deref(),
                ),
                _ => true,
            })
            .flat_map(|(_, item)| conversation_item_to_input_items(item))
            .collect()
    } else {
        req.items
            .iter()
            .flat_map(conversation_item_to_input_items)
            .collect()
    };

    // gx: the ChatGPT/Codex endpoint 400s ("System messages are not allowed")
    // on any input item carrying role:"system" (spike, verified live). Every
    // real gx request sends its system prompt as a system-role input item, so
    // under codex_compat those items are rewritten to role:"developer" —
    // empirically accepted (200) and semantically equivalent (both roles
    // outrank "user" in the Responses API's instruction hierarchy). Left
    // alone off the codex path to avoid an xAI regression.
    //
    // The same endpoint 400s on `id: ""` (and any id with characters outside
    // `[A-Za-z0-9_-]`): "Invalid 'input[N].id': ''. Expected an ID that
    // contains letters, numbers, underscores, or dashes". Chat-completions
    // providers (GLM, Fireworks, …) synthesize reasoning items with an empty
    // id (`synthesized_reasoning_item`); switching to an openai-codex model
    // mid-session then replays them and every turn 400s. `rs::ReasoningItem.id`
    // is a required `String`, so the empty value cannot be omitted through the
    // typed mapping — drop those items instead. A live POST that omitted the
    // id field *or* dropped the item both returned 200; dropping is what the
    // typed API can do.
    //
    // Sealed `encrypted_content` is decryptable only by the backend that
    // minted it. Provenance is the following `AssistantItem.model_id`
    // (stamped from `response.model` at ingest) compared to `req.model` —
    // not the id prefix. xAI/grok-build items have used `rs_*` ids
    // (`rs_grokbuild_legacy` in `test_sampling_client`), so a prefix
    // heuristic would strip same-family Grok replay off this path and
    // keep foreign blobs on Codex. Unknown-provenance sealed items are
    // dropped on Codex (cannot prove decryptable). Left alone off the
    // codex path so xAI round-trips stay byte-stable.
    if req.codex_compat {
        for item in &mut items {
            if let rs::InputItem::EasyMessage(msg) = item
                && msg.role == rs::Role::System
            {
                msg.role = rs::Role::Developer;
            }
        }
        items.retain(|item| match item {
            rs::InputItem::Item(rs::Item::Reasoning(r)) => is_codex_item_id(&r.id),
            _ => true,
        });
    }

    rs::InputParam::Items(items)
}

/// gx: keep a reasoning sibling on the Codex wire only when its id is
/// legal *and* any sealed blob is attributable to this request's model.
fn keep_codex_reasoning(
    r: &rs::ReasoningItem,
    following_assistant_model: Option<&str>,
    req_model: Option<&str>,
) -> bool {
    if !is_codex_item_id(&r.id) {
        return false;
    }
    if r.encrypted_content.is_none() {
        return true;
    }
    matches!(
        (following_assistant_model, req_model),
        (Some(got), Some(want)) if got == want
    )
}

/// gx: nearest later assistant in this turn. Skip other reasoning /
/// backend-tool siblings; stop at user/system/tool-result.
fn following_assistant_model_id(items: &[ConversationItem], from: usize) -> Option<&str> {
    for item in items.iter().skip(from.saturating_add(1)) {
        match item {
            ConversationItem::Reasoning(_) | ConversationItem::BackendToolCall(_) => continue,
            ConversationItem::Assistant(a) => return a.model_id.as_deref(),
            _ => return None,
        }
    }
    None
}

/// gx: Codex input-item ids are non-empty `[A-Za-z0-9_-]`; empty fails live.
pub(crate) fn is_codex_item_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Inject the `type: "reasoning_text"` discriminator the API requires.
/// `async-openai`'s `ReasoningTextContent` has no `type` field, so it serializes to `{"text": ...}` and the API answers 400.
/// Delete this once upstream grows the field.
pub fn patch_reasoning_text_types(body: &mut serde_json::Value) {
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for item in input.iter_mut() {
        if item.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for c in content.iter_mut() {
            if let Some(obj) = c.as_object_mut() {
                obj.entry("type")
                    .or_insert_with(|| serde_json::Value::String("reasoning_text".into()));
            }
        }
    }
}

fn conversation_item_to_input_items(item: &ConversationItem) -> Vec<rs::InputItem> {
    match item {
        ConversationItem::System(s) => {
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::System,
                content: rs::EasyInputContent::Text(s.content.as_ref().to_owned()),
            })]
        }
        ConversationItem::User(u) => {
            let content = content_parts_to_easy_input_content(&u.content);
            vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::User,
                content,
            })]
        }
        ConversationItem::Reasoning(r) => {
            // `status` is output-only and rejected on input.
            let mut r = r.clone();
            r.status = None;
            vec![rs::InputItem::Item(rs::Item::Reasoning(r))]
        }
        ConversationItem::Assistant(a) => {
            let mut items = Vec::new();

            if !a.content.is_empty() {
                items.push(rs::InputItem::EasyMessage(rs::EasyInputMessage {
                    r#type: rs::MessageType::Message,
                    role: rs::Role::Assistant,
                    content: rs::EasyInputContent::Text(a.content.as_ref().to_owned()),
                }));
            }

            for tc in &a.tool_calls {
                let arguments = sanitize_tool_arguments(&tc.id, &tc.name, tc.arguments.clone());
                items.push(rs::InputItem::Item(rs::Item::FunctionCall(
                    rs::FunctionToolCall {
                        call_id: tc.id.as_ref().to_owned(),
                        name: tc.name.clone(),
                        arguments: arguments.as_ref().to_owned(),
                        id: None,
                        status: None,
                    },
                )));
            }

            items
        }
        ConversationItem::ToolResult(t) => {
            let output = if t.images.is_empty() {
                rs::FunctionCallOutput::Text(t.content.as_ref().to_owned())
            } else {
                let mut parts: Vec<rs::InputContent> =
                    vec![rs::InputContent::InputText(rs::InputTextContent {
                        text: t.content.as_ref().to_owned(),
                    })];
                for img in &t.images {
                    if let ContentPart::Image { url } = img {
                        parts.push(rs::InputContent::InputImage(rs::InputImageContent {
                            detail: rs::ImageDetail::Auto,
                            file_id: None,
                            image_url: Some(url.as_ref().to_owned()),
                        }));
                    }
                }
                rs::FunctionCallOutput::Content(parts)
            };
            vec![rs::InputItem::Item(rs::Item::FunctionCallOutput(
                rs::FunctionCallOutputItemParam {
                    call_id: t.tool_call_id.clone(),
                    output,
                    id: None,
                    status: None,
                },
            ))]
        }
        ConversationItem::BackendToolCall(b) => {
            vec![match &b.kind {
                BackendToolKind::WebSearch(ws) => {
                    rs::InputItem::Item(rs::Item::WebSearchCall(ws.clone()))
                }
                BackendToolKind::XSearch(ct) => {
                    rs::InputItem::Item(rs::Item::CustomToolCall(ct.clone()))
                }
                BackendToolKind::CodeInterpreter(ci) => {
                    rs::InputItem::Item(rs::Item::CodeInterpreterCall(ci.clone()))
                }
            }]
        }
    }
}

fn content_parts_to_easy_input_content(parts: &[ContentPart]) -> rs::EasyInputContent {
    if parts.len() == 1
        && let ContentPart::Text { text } = &parts[0]
    {
        return rs::EasyInputContent::Text(text.as_ref().to_owned());
    }

    let items: Vec<rs::InputContent> = parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => rs::InputContent::InputText(rs::InputTextContent {
                text: text.as_ref().to_owned(),
            }),
            ContentPart::Image { url } => rs::InputContent::InputImage(rs::InputImageContent {
                image_url: Some(url.as_ref().to_owned()),
                file_id: None,
                detail: rs::ImageDetail::default(),
            }),
        })
        .collect();

    rs::EasyInputContent::ContentList(items)
}

/// The request's client function tools.
/// A function tool whose name collides with a backend-hosted tool is dropped: sending both is rejected as a duplicate, so the hosted tool wins.
/// Both ride the raw-JSON [`extra_tool_entries`] channel instead.
fn build_responses_tools(req: &ConversationRequest) -> Vec<rs::Tool> {
    let tools: Vec<rs::Tool> = req
        .tools
        .iter()
        .filter(|t| {
            let collides = req.hosted_tools.iter().any(|h| h.wire_name() == t.name);
            if collides {
                tracing::warn!(
                    tool = %t.name,
                    "dropping function tool that collides with a backend-hosted tool"
                );
            }
            !collides
        })
        .map(|t| {
            rs::Tool::Function(rs::FunctionTool {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: Some(t.parameters.clone()),
                strict: None,
            })
        })
        .collect();

    tools
}

/// Every hosted tool as a raw JSON entry, which the sampler client splices into the serialized `tools` array.
/// `web_search` rides it because async_openai's `rs::WebSearchToolFilters` models only `allowed_domains` and cannot carry `excluded_domains`.
/// Emitting either as a typed `rs::Tool` as well would send it twice, which the API rejects as a duplicate.
pub fn extra_tool_entries(hosted_tools: &[HostedTool]) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    for tool in hosted_tools {
        match tool {
            HostedTool::WebSearch { options } => {
                entries.push(match options {
                    Some(o) => o.to_tool_entry(),
                    None => WebSearchOptions::default().to_tool_entry(),
                });
            }
            HostedTool::XSearch { options } => {
                entries.push(match options {
                    Some(o) => o.to_tool_entry(),
                    None => XSearchOptions::default().to_tool_entry(),
                });
            }
        }
    }
    entries
}
