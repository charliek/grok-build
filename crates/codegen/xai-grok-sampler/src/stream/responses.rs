//! Layer-2 stream transform for the OpenAI Responses API.
//!
//! Consumes a raw `rs::ResponseStreamEvent` stream and produces
//! [`SamplingEvent`]s. Pure: no I/O, no shell coupling.

use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};

use xai_grok_sampling_types::{
    ConversationItem, ConversationResponse, ResponseModelMetadata, SamplingError, StopReason,
    TokenUsage, rs,
};

use crate::doom_loop_recovery::FailedResponseCapture;
use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// Hard cap on the number of items retained in `streamed_output_items` (the
/// empty-terminal-frame fallback; see its doc comment). A real turn's output
/// array is small — a handful of messages/tool calls — so this is generous
/// headroom, not a working limit. It exists purely to bound memory for a
/// runaway or adversarial stream: once hit, further items are dropped and
/// the fallback simply becomes partial for that turn, which is strictly
/// better than growing without bound. The normal `response.output` path
/// (the common case) is unaffected either way.
const MAX_STREAMED_OUTPUT_ITEMS: usize = 512;

/// Returns whether a Responses API event reflects real model progress
/// rather than a liveness-only heartbeat / status transition.
pub(crate) fn responses_event_has_meaningful_content(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent;

    match event {
        ResponseStreamEvent::ResponseCreated(_)
        | ResponseStreamEvent::ResponseInProgress(_)
        | ResponseStreamEvent::ResponseQueued(_) => false,
        ResponseStreamEvent::ResponseOutputTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseOutputTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseRefusalDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseRefusalDone(event) => !event.refusal.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDone(event) => {
            !event.arguments.is_empty() || event.name.as_ref().is_some_and(|name| !name.is_empty())
        }
        ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningSummaryTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDone(event) => !event.arguments.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDone(event) => !event.code.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDone(event) => !event.input.is_empty(),
        ResponseStreamEvent::ResponseFailed(event) => {
            !event.response.output.is_empty()
                || event
                    .response
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.output_tokens > 0)
        }
        ResponseStreamEvent::ResponseCompleted(_)
        | ResponseStreamEvent::ResponseIncomplete(_)
        | ResponseStreamEvent::ResponseOutputItemAdded(_)
        | ResponseStreamEvent::ResponseOutputItemDone(_)
        | ResponseStreamEvent::ResponseContentPartAdded(_)
        | ResponseStreamEvent::ResponseContentPartDone(_)
        | ResponseStreamEvent::ResponseFileSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseFileSearchCallSearching(_)
        | ResponseStreamEvent::ResponseFileSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseWebSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseWebSearchCallSearching(_)
        | ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartAdded(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartDone(_)
        | ResponseStreamEvent::ResponseImageGenerationCallCompleted(_)
        | ResponseStreamEvent::ResponseImageGenerationCallGenerating(_)
        | ResponseStreamEvent::ResponseImageGenerationCallInProgress(_)
        | ResponseStreamEvent::ResponseImageGenerationCallPartialImage(_)
        | ResponseStreamEvent::ResponseMCPCallCompleted(_)
        | ResponseStreamEvent::ResponseMCPCallFailed(_)
        | ResponseStreamEvent::ResponseMCPCallInProgress(_)
        | ResponseStreamEvent::ResponseMCPListToolsCompleted(_)
        | ResponseStreamEvent::ResponseMCPListToolsFailed(_)
        | ResponseStreamEvent::ResponseMCPListToolsInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_)
        | ResponseStreamEvent::ResponseOutputTextAnnotationAdded(_)
        | ResponseStreamEvent::ResponseError(_) => true,
    }
}

pub(crate) fn responses_event_may_have_output(event: &rs::ResponseStreamEvent) -> bool {
    !matches!(event, rs::ResponseStreamEvent::ResponseError(_))
        && responses_event_has_meaningful_content(event)
}

/// Copy everything the Doom-loop capture needs out of a frame.
///
/// This is the single observation point: it runs for every frame *before* the
/// abort gate, so the frame a confident signal aborts on is observed exactly
/// like any other. Two things matter — a completed item is the authoritative
/// copy of what the deltas approximated, and any frame that names tool
/// activity or compaction state vetoes the replay, since reasoning must never
/// be retried without the item it is bound to.
fn observe_for_recovery(capture: &FailedResponseCapture, event: &rs::ResponseStreamEvent) {
    use rs::ResponseStreamEvent as Event;
    if !capture.is_armed() {
        return;
    }
    match event {
        Event::ResponseOutputTextDelta(text) => capture.record_output_delta(
            text.output_index,
            text.content_index,
            text.item_id.clone(),
            &text.delta,
        ),
        Event::ResponseOutputTextDone(text) => capture.record_output_done(
            text.output_index,
            text.content_index,
            text.item_id.clone(),
            text.text.clone(),
        ),
        Event::ResponseReasoningTextDelta(reasoning) => capture.record_reasoning_delta(
            reasoning.output_index,
            reasoning.content_index,
            reasoning.item_id.clone(),
            &reasoning.delta,
        ),
        Event::ResponseReasoningTextDone(reasoning) => capture.record_reasoning_done(
            reasoning.output_index,
            reasoning.content_index,
            reasoning.item_id.clone(),
            reasoning.text.clone(),
        ),
        Event::ResponseReasoningSummaryTextDelta(summary) => capture
            .record_reasoning_summary_delta(
                summary.output_index,
                summary.summary_index,
                summary.item_id.clone(),
                &summary.delta,
            ),
        Event::ResponseReasoningSummaryTextDone(summary) => capture.record_reasoning_summary_done(
            summary.output_index,
            summary.summary_index,
            summary.item_id.clone(),
            summary.text.clone(),
        ),
        Event::ResponseOutputItemAdded(added) => capture.record_item_start(&added.item),
        Event::ResponseOutputItemDone(done) => {
            capture.record_output_item(done.output_index, &done.item);
        }
        Event::ResponseCompleted(completed) => {
            capture.record_terminal_output(&completed.response.output);
        }
        Event::ResponseIncomplete(incomplete) => {
            capture.record_terminal_output(&incomplete.response.output);
        }
        // Frames that only name in-flight tool work. The item they belong to
        // may never complete on this attempt, so the frame itself is the
        // notice that a call was in flight.
        Event::ResponseFunctionCallArgumentsDelta(_)
        | Event::ResponseFunctionCallArgumentsDone(_)
        | Event::ResponseCustomToolCallInputDelta(_)
        | Event::ResponseCustomToolCallInputDone(_)
        | Event::ResponseCodeInterpreterCallCodeDelta(_)
        | Event::ResponseCodeInterpreterCallCodeDone(_)
        | Event::ResponseCodeInterpreterCallInProgress(_)
        | Event::ResponseCodeInterpreterCallInterpreting(_)
        | Event::ResponseCodeInterpreterCallCompleted(_)
        | Event::ResponseFileSearchCallInProgress(_)
        | Event::ResponseFileSearchCallSearching(_)
        | Event::ResponseFileSearchCallCompleted(_)
        | Event::ResponseWebSearchCallInProgress(_)
        | Event::ResponseWebSearchCallSearching(_)
        | Event::ResponseWebSearchCallCompleted(_)
        | Event::ResponseImageGenerationCallInProgress(_)
        | Event::ResponseImageGenerationCallGenerating(_)
        | Event::ResponseImageGenerationCallCompleted(_)
        | Event::ResponseMCPCallInProgress(_)
        | Event::ResponseMCPCallCompleted(_)
        | Event::ResponseMCPCallFailed(_)
        | Event::ResponseMCPCallArgumentsDelta(_)
        | Event::ResponseMCPCallArgumentsDone(_) => capture.record_unreplayable(),
        _ => {}
    }
}

/// Transform a raw Responses API event stream into a stream of
/// [`SamplingEvent`]s.
///
/// Yields exactly one terminal event ([`SamplingEvent::Completed`] or
/// [`SamplingEvent::Failed`]) per request. Server-side `ResponseFailed`
/// and `ResponseError` events are translated to
/// `SamplingError::Api { status: 500, .. }` so the actor's retry loop
/// treats them as retryable.
///
/// `doom_loop` is the collector returned alongside `raw_stream` by
/// `SamplingClient::conversation_stream_responses`; any signals the SSE
/// decoder recorded are drained onto the final `ConversationResponse`.
/// `None` (check disabled) leaves the response untouched.
pub fn stream_responses<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream_responses_tracked(
        raw_stream,
        model_metadata,
        request_id,
        idle_timeout,
        doom_loop,
        Arc::new(AtomicBool::new(false)),
        FailedResponseCapture::default(),
    )
}

pub(crate) fn stream_responses_tracked<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
    output_observed: Arc<AtomicBool>,
    failed_response: FailedResponseCapture,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    async_stream::stream! {
        use rs::{ResponseStreamEvent, Status};

        let stream_start = Instant::now();
        let mut chunk_timestamps: Vec<Instant> = Vec::new();

        yield SamplingEvent::StreamStarted {
            request_id: request_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        if let Some(metadata) = model_metadata {
            yield SamplingEvent::ModelMetadata {
                request_id: request_id.clone(),
                metadata,
            };
        }

        let mut final_response: Option<rs::Response> = None;
        let mut chunk_index: u64 = 0;
        let mut message_chunk_count: u64 = 0;
        let mut first_token_emitted = false;
        let mut reasoning_acc = String::new();
        let mut last_content_chunk_at = Instant::now();

        // Maps Responses API `output_index` to our tool-only `tool_index`.
        // Populated when `ResponseOutputItemAdded` carries a `FunctionCall`;
        // later `ResponseFunctionCallArgumentsDelta` events
        // look up `output_index` here to find the matching `tool_index`.
        let mut output_to_tool_index: BTreeMap<u32, u32> = BTreeMap::new();
        let mut next_tool_index: u32 = 0;

        // Completed output items observed on the wire, keyed by
        // `output_index` so they replay in server order.
        //
        // The Responses API contract says the terminal `response.completed` /
        // `response.incomplete` frame repeats the full `output` array, and
        // that is what we normally read. Some backends (notably the ChatGPT
        // `backend-api/codex` deployment) send the terminal frame with
        // `"output": []` and expect the client to have assembled the turn
        // from the per-item `response.output_item.done` frames instead.
        // Without this, every visible message and tool call is dropped and
        // the turn looks empty even though usage was billed.
        //
        // Only variants that `response_to_conversation_items` actually turns
        // into a `ConversationItem` are worth keeping around for this
        // fallback (see the insert site below for the exact list and why).
        // Everything else — including `McpCall`, whose payloads can be
        // large — is dropped at insert time rather than retained until the
        // terminal frame, and insertion stops past
        // `MAX_STREAMED_OUTPUT_ITEMS` regardless of variant, so a runaway
        // stream can't grow this without bound.
        let mut streamed_output_items: BTreeMap<u32, rs::OutputItem> = BTreeMap::new();

        let mut stream = raw_stream;
        loop {
            let event_result = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(event_result)) => event_result,
                Ok(None) => break,
                Err(_elapsed) => {
                    let err = SamplingError::IdleTimeout {
                        elapsed_secs: idle_timeout.as_secs(),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            let event = match event_result {
                Ok(event) => event,
                Err(err) => {
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            if responses_event_may_have_output(&event) {
                output_observed.store(true, Ordering::Relaxed);
            }

            // A confident midstream signal aborts the attempt immediately.
            // Terminal frames are processed so their complete response items
            // remain available to the retry loop; `drive_l2` rejects the
            // completed response before it can be accepted.
            let is_terminal_response = matches!(
                &event,
                ResponseStreamEvent::ResponseCompleted(_)
                    | ResponseStreamEvent::ResponseIncomplete(_)
            );
            // Observed before the abort gate so the aborting frame lands in
            // the capture like any other; the attempt is discarded either
            // way, so nothing here is surfaced downstream.
            observe_for_recovery(&failed_response, &event);

            if !is_terminal_response
                && let Some(triggers) = doom_loop.as_ref().and_then(|c| c.abort_triggers())
            {
                let err = SamplingError::DoomLoopDetected {
                    triggers,
                    aborted_at_chunk: Some(chunk_index),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            let event_has_content = responses_event_has_meaningful_content(&event);

            // Track whether ResponseIncomplete should break the loop
            // after the content-aware idle check below.
            let mut should_break = false;

            match event {
                ResponseStreamEvent::ResponseOutputTextDelta(text_delta_event) => {
                    let delta = text_delta_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_timestamps.push(Instant::now());
                        chunk_index += 1;
                        message_chunk_count += 1;
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Text,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningSummaryTextDelta(summary_event) => {
                    let delta = summary_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningTextDelta(reasoning_event) => {
                    let delta = reasoning_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        reasoning_acc.push_str(&delta);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                // Start of a Responses FunctionCall — emit initial id+name
                // and remember the output_index → tool_index mapping.
                ResponseStreamEvent::ResponseOutputItemAdded(added_event) => {
                    if let rs::OutputItem::FunctionCall(fc) = added_event.item {
                        let tool_index = next_tool_index;
                        next_tool_index += 1;
                        output_to_tool_index.insert(added_event.output_index, tool_index);

                        yield SamplingEvent::ToolCallDelta {
                            request_id: request_id.clone(),
                            tool_index,
                            id: Some(fc.call_id),
                            name: Some(fc.name),
                            arguments_delta: None,
                        };
                    }
                }

                // Continuation chunk for a streaming FunctionCall's args.
                // Drop silently if no preceding OutputItemAdded mapped.
                ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(args_event) => {
                    let delta = args_event.delta;
                    if !delta.is_empty()
                        && let Some(&tool_index) =
                            output_to_tool_index.get(&args_event.output_index)
                    {
                        yield SamplingEvent::ToolCallDelta {
                            request_id: request_id.clone(),
                            tool_index,
                            id: None,
                            name: None,
                            arguments_delta: Some(delta),
                        };
                    }
                }

                ResponseStreamEvent::ResponseCompleted(completed_event) => {
                    final_response = Some(completed_event.response);
                }

                ResponseStreamEvent::ResponseIncomplete(incomplete_event) => {
                    final_response = Some(incomplete_event.response);
                    should_break = true;
                }

                ResponseStreamEvent::ResponseFailed(failed_event) => {
                    let response = failed_event.response;
                    let error_message = response
                        .error
                        .as_ref()
                        .map(|e| format!("{}: {}", e.code, e.message))
                        .unwrap_or_else(|| "Response failed with unknown error".to_string());
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                        error_code: response
                            .error
                            .as_ref()
                            .map(|e| xai_grok_sampling_types::ApiErrorCode::parse(&e.code)),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                ResponseStreamEvent::ResponseError(error_event) => {
                    let error_message = format!(
                        "{}: {}",
                        error_event.code.as_deref().unwrap_or("error"),
                        error_event.message
                    );
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                        // The wire code, absent when the event carried none.
                        error_code: error_event
                            .code
                            .as_deref()
                            .map(xai_grok_sampling_types::ApiErrorCode::parse),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                // ── Backend-hosted tool lifecycle events ────────────
                // These tools are executed server-side by the agentic
                // sampler. We emit progress events so the shell/pager
                // can show status to the user.

                // Web search
                ResponseStreamEvent::ResponseWebSearchCallInProgress(ev) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "web_search".to_string(),
                    };
                }
                // Completed/Searching carry no data — the real payload
                // arrives via ResponseOutputItemDone(WebSearchCall) below.
                ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
                | ResponseStreamEvent::ResponseWebSearchCallSearching(_) => {}

                // Code interpreter (server-side, like web/x search). Surface it
                // the same way x_search is: a generic backend tool call that the
                // shell renders as a client `tool_use` + `user` `tool_result`
                // split (grok has no HostedTool::CodeInterpreter, so these events
                // are latent under the current hosted-tool set). The started
                // event fires on InProgress; the full payload (code + outputs)
                // rides ResponseOutputItemDone(CodeInterpreterCall) below.
                ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(ev) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "code_interpreter".to_string(),
                    };
                }
                // Interpreting/Completed carry no payload — the result arrives
                // via ResponseOutputItemDone(CodeInterpreterCall) below.
                ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
                | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_) => {}

                // OutputItemDone carries the full result for backend tools.
                // For WebSearchCall this includes the query and source URLs.
                // For CustomToolCall this includes x_search results.
                ResponseStreamEvent::ResponseOutputItemDone(done_event) => {
                    match &done_event.item {
                        rs::OutputItem::WebSearchCall(ws) => {
                            let result = serde_json::to_value(ws).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ws.id.clone(),
                                name: "web_search".to_string(),
                                result,
                            };
                        }
                        // X search results arrive as CustomToolCall with
                        // names like x_keyword_search, x_semantic_search, etc.
                        // Use "x_search" consistently (matching the Started event);
                        // the specific sub-type is in the serialized result payload
                        // and extracted by the pager from raw_output.name.
                        rs::OutputItem::CustomToolCall(ct) => {
                            let result = serde_json::to_value(ct).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ct.id.clone(),
                                name: "x_search".to_string(),
                                result,
                            };
                        }
                        // Code interpreter: the full call (code + outputs) rides
                        // the done item. Surfaced under the shared "code_interpreter"
                        // name (matching the Started event); the shell renders it via
                        // the client `tool_use` + `user` `tool_result` split.
                        rs::OutputItem::CodeInterpreterCall(ci) => {
                            let result = serde_json::to_value(ci).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ci.id.clone(),
                                name: "code_interpreter".to_string(),
                                result,
                            };
                        }
                        _ => {}
                    }
                    // Authoritative per-item copy of what the deltas
                    // approximated. Kept as a fallback for backends whose
                    // terminal frame omits `output`; see
                    // `streamed_output_items`. Only store variants that
                    // `response_to_conversation_items` actually converts
                    // into a `ConversationItem` — Message (assistant text),
                    // FunctionCall (tool calls), Reasoning, and the three
                    // backend-tool-call kinds it renders as
                    // `BackendToolCall` items (WebSearchCall, CustomToolCall,
                    // CodeInterpreterCall). Every other variant (McpCall
                    // included — it only ever bumps a counter there, never
                    // producing an item, despite sometimes carrying a large
                    // payload) is dropped here rather than retained until
                    // the terminal frame. Bounded by
                    // `MAX_STREAMED_OUTPUT_ITEMS` so a runaway stream can't
                    // grow this without limit.
                    if streamed_output_items.len() < MAX_STREAMED_OUTPUT_ITEMS {
                        match &done_event.item {
                            rs::OutputItem::Message(_)
                            | rs::OutputItem::FunctionCall(_)
                            | rs::OutputItem::Reasoning(_)
                            | rs::OutputItem::WebSearchCall(_)
                            | rs::OutputItem::CustomToolCall(_)
                            | rs::OutputItem::CodeInterpreterCall(_) => {
                                streamed_output_items
                                    .insert(done_event.output_index, done_event.item);
                            }
                            _ => {}
                        }
                    }
                }

                // CustomToolCallInputDelta is x_search in-progress streaming.
                // Emit a started event on first delta per item_id.
                ResponseStreamEvent::ResponseCustomToolCallInputDone(ev) => {
                    yield SamplingEvent::BackendToolCallStarted {
                        request_id: request_id.clone(),
                        call_id: ev.item_id.clone(),
                        name: "x_search".to_string(),
                    };
                }

                // All other events (intermediate progress, annotations,
                // image gen, file search, etc.) — no action needed.
                _ => {}
            }

            if event_has_content {
                last_content_chunk_at = Instant::now();
            } else if last_content_chunk_at.elapsed() > idle_timeout {
                let err = SamplingError::IdleTimeout {
                    elapsed_secs: idle_timeout.as_secs(),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            if should_break {
                break;
            }
        }

        // ── Build the final response ─────────────────────────────────
        let mut response = match final_response {
            Some(r) => r,
            None => {
                let err = SamplingError::Api {
                    status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    message: "No ResponseCompleted or ResponseIncomplete event received from \
                              Responses API"
                        .to_string(),
                    model_metadata: None,
                    retry_after_secs: None,
                    should_retry: None,
                    // Synthesized client-side; no wire envelope to read.
                    error_code: None,
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }
        };

        // Billing fields (`prompt_tokens`, `completion_tokens`,
        // `cached_prompt_tokens`, `reasoning_tokens`) are the cumulative
        // wire values — they sum across every server-side turn of the
        // agent loop and are what we bill on / log to telemetry.
        //
        // `total_tokens` is the live context length used to drive the
        // CLI `/context` bar, the auto-compact threshold, and
        // `meta.totalTokens` on persisted sessions. The SSE decoder
        // (`deserialize_response_event`) has already rewritten
        // `u.total_tokens` to `context_details.input + output` when
        // the backend emits it; on older deployments the wire
        // value passes through unchanged.
        let usage = response.usage.as_ref().map(|u| TokenUsage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
            reasoning_tokens: u.output_tokens_details.reasoning_tokens,
            cached_prompt_tokens: u.input_tokens_details.cached_tokens,
            cache_creation_prompt_tokens: 0,
        });

        let cost_usd_ticks = response
            .metadata
            .as_mut()
            .and_then(|m| m.remove(crate::client::COST_USD_TICKS_METADATA_KEY))
            .and_then(|s| s.parse::<i64>().ok());

        let status = response.status.clone();

        // Terminal frame carried no output array: rebuild the turn from the
        // `response.output_item.done` frames we saw. Only ever a fallback —
        // when the terminal frame does carry `output` it stays authoritative.
        if response.output.is_empty() && !streamed_output_items.is_empty() {
            response.output = std::mem::take(&mut streamed_output_items)
                .into_values()
                .collect();
        }

        // Convert to ConversationItem(s); patch in accumulated reasoning
        // text as a fallback when the final response lacks `content` /
        // `summary` (the streaming deltas may have arrived out of band).
        // Splice policy lives in `inject_streaming_reasoning_fallback`.
        let mut items = xai_grok_sampling_types::response_to_conversation_items(response);
        xai_grok_sampling_types::inject_streaming_reasoning_fallback(&mut items, reasoning_acc);

        let has_tool_calls = items.iter().any(|i| match i {
            ConversationItem::Assistant(a) => !a.tool_calls.is_empty(),
            _ => false,
        });

        let stop_reason = if has_tool_calls {
            Some(StopReason::ToolCalls)
        } else {
            match status {
                Status::Completed => Some(StopReason::Stop),
                Status::Incomplete => Some(StopReason::Length),
                _ => None,
            }
        };

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);

        // Warn-only for now: surface the server-reported triggers once per
        // request (raw labels only — ZDR-safe) and attach them for callers.
        let doom_loop_signals = doom_loop
            .as_ref()
            .map(|collector| collector.take())
            .unwrap_or_default();
        if !doom_loop_signals.is_empty() {
            tracing::warn!(
                request_id = %request_id,
                triggers = ?doom_loop_signals.iter().map(|s| s.raw.as_str()).collect::<Vec<_>>(),
                "server reported doom-loop triggers for this response"
            );
        }

        let conversation_response = ConversationResponse {
            items,
            stop_reason,
            usage,
            cost_usd_ticks,
            message_chunks_emitted: message_chunk_count,
            doom_loop_signals,
            stop_message: None, // not reported on the Responses API
            message_id: None,   // no provider message id on the Responses API
            raw_stop_reason: None,
            stop_sequence: None,
        };

        yield SamplingEvent::Completed {
            request_id: request_id.clone(),
            response: Box::new(conversation_response),
            metrics,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses as rs_types;
    use futures_util::stream;
    use std::pin::pin;

    fn rid() -> RequestId {
        RequestId::from("resp-test")
    }

    /// Build a minimal `rs_types::Response` for use in `ResponseCompleted`
    fn build_response(status: rs_types::Status) -> rs_types::Response {
        rs_types::Response {
            background: None,
            billing: None,
            conversation: None,
            created_at: 0,
            completed_at: None,
            error: None,
            id: "resp_1".into(),
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            metadata: None,
            model: "test-model".into(),
            object: "response".into(),
            output: vec![],
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: None,
            status,
            temperature: None,
            text: None,
            tool_choice: None,
            tools: None,
            top_logprobs: None,
            top_p: None,
            truncation: None,
            usage: None,
        }
    }

    fn empty_completed_response() -> rs_types::Response {
        build_response(rs_types::Status::Completed)
    }

    fn failed_response_with_error(message: &str) -> rs_types::Response {
        let mut r = build_response(rs_types::Status::Failed);
        r.error = Some(rs_types::ErrorObject {
            code: "server_error".into(),
            message: message.into(),
        });
        r
    }

    fn text_delta_event(delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputTextDelta(rs_types::ResponseTextDeltaEvent {
            sequence_number: 0,
            item_id: "item-1".into(),
            output_index: 0,
            content_index: 0,
            delta: delta.into(),
            logprobs: None,
        })
    }

    fn completed_event() -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
            response: empty_completed_response(),
            sequence_number: 0,
        })
    }

    async fn collect(s: impl Stream<Item = SamplingEvent>) -> Vec<SamplingEvent> {
        let mut out = Vec::new();
        let mut s = pin!(s);
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    /// A confident signal that aborts on a custom-tool input frame still
    /// vetoes the replay: the frame is the only notice that a call was in
    /// flight, and reasoning must never be retried without it. The same holds
    /// for the code-interpreter code frames.
    #[tokio::test]
    async fn an_abort_on_a_tool_input_frame_vetoes_the_replay() {
        for tool_frame in [
            rs::ResponseStreamEvent::ResponseCustomToolCallInputDelta(
                rs_types::ResponseCustomToolCallInputDeltaEvent {
                    sequence_number: 1,
                    output_index: 1,
                    item_id: "custom-1".into(),
                    delta: "{\"q\":".into(),
                },
            ),
            rs::ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(
                rs_types::ResponseCodeInterpreterCallCodeDeltaEvent {
                    sequence_number: 1,
                    output_index: 1,
                    item_id: "ci-1".into(),
                    delta: "print(".into(),
                },
            ),
        ] {
            let capture = FailedResponseCapture::armed();
            // A collector that has already seen a confident trigger: the next
            // non-terminal frame aborts the attempt.
            let collector = crate::doom_loop::DoomLoopSignalCollector::new(
                xai_grok_sampling_types::DoomLoopRecoveryPolicy::default(),
            );
            collector.absorb(
                xai_grok_sampling_types::doom_loop::DOOM_LOOP_CHECK_EVENT_TYPE,
                r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#,
            );

            // Reasoning already captured, so an intact replay would carry it:
            // only the veto can empty the capture. The collector is armed
            // before the stream runs, so the abort lands on the tool frame.
            capture.record_reasoning_delta(0, 0, "reasoning-1".into(), "looping thought");
            let raw = stream::iter(vec![Ok(tool_frame), Ok(completed_event())]).boxed();
            let events = collect(stream_responses_tracked(
                raw,
                None,
                rid(),
                Duration::from_secs(60),
                Some(collector),
                Arc::new(AtomicBool::new(false)),
                capture.clone(),
            ))
            .await;

            assert!(
                matches!(events.last(), Some(SamplingEvent::Failed { .. })),
                "the confident signal aborts the attempt"
            );
            assert!(
                capture.take_items().is_empty(),
                "a turn with a call in flight replays nothing"
            );
        }
    }

    #[tokio::test]
    async fn missing_completed_event_yields_failed() {
        let raw =
            stream::iter(Vec::<Result<rs::ResponseStreamEvent, SamplingError>>::new()).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_delta_then_completed_yields_completed_with_stop() {
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let text_tokens: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Text,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_tokens, vec!["hello"]);

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Parse a raw SSE `data:` payload exactly as it arrives on the wire.
    fn wire_event(json: &str) -> rs::ResponseStreamEvent {
        serde_json::from_str(json).expect("wire event should deserialize")
    }

    /// Terminal frame from the ChatGPT `backend-api/codex` deployment: the
    /// `output` array is empty even for a successful turn, so the completed
    /// items only ever appear on `response.output_item.done`.
    fn codex_completed_frame(output_tokens: u32) -> String {
        format!(
            r#"{{"type":"response.completed","sequence_number":9,"response":{{
                "id":"resp_1","object":"response","created_at":1787629216,
                "completed_at":1787629217,"status":"completed","background":false,
                "error":null,"incomplete_details":null,"instructions":null,
                "max_output_tokens":null,"model":"gpt-5.6-sol","output":[],
                "parallel_tool_calls":false,"previous_response_id":null,
                "temperature":1.0,"tool_choice":"auto","tools":[],"top_p":1.0,
                "usage":{{"input_tokens":10,"input_tokens_details":{{"cached_tokens":0}},
                "output_tokens":{output_tokens},
                "output_tokens_details":{{"reasoning_tokens":0}},"total_tokens":{total}}}
            }}}}"#,
            total = 10 + output_tokens,
        )
    }

    fn completed_response(events: &[SamplingEvent]) -> &ConversationResponse {
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => response,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    fn assistant_items(
        response: &ConversationResponse,
    ) -> Vec<&xai_grok_sampling_types::AssistantItem> {
        response
            .items
            .iter()
            .filter_map(|item| match item {
                ConversationItem::Assistant(a) => Some(a),
                _ => None,
            })
            .collect()
    }

    /// Replays the real `gpt-5.6-sol` "Reply with exactly pong" turn recorded
    /// off `https://chatgpt.com/backend-api/codex`. The terminal frame's
    /// `output` is `[]`, so the visible text has to come from the
    /// `response.output_item.done` frame.
    #[tokio::test]
    async fn completed_frame_without_output_recovers_message_from_item_done() {
        let raw = stream::iter(vec![
            Ok(wire_event(
                r#"{"type":"response.output_item.added","sequence_number":2,"output_index":0,
                    "item":{"id":"msg_1","type":"message","status":"in_progress","content":[],
                    "phase":"final_answer","role":"assistant"}}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.content_part.added","sequence_number":3,"item_id":"msg_1",
                    "output_index":0,"content_index":0,
                    "part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_1",
                    "output_index":0,"content_index":0,"delta":"pong","logprobs":[]}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.output_text.done","sequence_number":5,"item_id":"msg_1",
                    "output_index":0,"content_index":0,"text":"pong","logprobs":[]}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.content_part.done","sequence_number":6,"item_id":"msg_1",
                    "output_index":0,"content_index":0,
                    "part":{"type":"output_text","annotations":[],"logprobs":[],"text":"pong"}}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.output_item.done","sequence_number":7,"output_index":0,
                    "item":{"id":"msg_1","type":"message","status":"completed",
                    "content":[{"type":"output_text","annotations":[],"logprobs":[],
                    "text":"pong"}],"phase":"final_answer","role":"assistant"}}"#,
            )),
            Ok(wire_event(&codex_completed_frame(5))),
        ])
        .boxed();

        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let response = completed_response(&events);
        let assistants = assistant_items(response);
        assert_eq!(assistants.len(), 1, "expected one assistant item");
        assert_eq!(&*assistants[0].content, "pong");
        assert!(assistants[0].tool_calls.is_empty());
        assert_eq!(response.stop_reason, Some(StopReason::Stop));
        assert_eq!(response.usage.as_ref().unwrap().completion_tokens, 5);
    }

    /// Same deployment, tool-call turn: the function call is only ever
    /// described by `response.output_item.done`.
    #[tokio::test]
    async fn completed_frame_without_output_recovers_function_call_from_item_done() {
        let raw = stream::iter(vec![
            Ok(wire_event(
                r#"{"type":"response.output_item.added","sequence_number":2,"output_index":0,
                    "item":{"id":"fc_1","type":"function_call","status":"in_progress",
                    "arguments":"","call_id":"call_abc","name":"get_marker"}}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.function_call_arguments.delta","sequence_number":3,
                    "item_id":"fc_1","output_index":0,"delta":"{}"}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.function_call_arguments.done","sequence_number":4,
                    "item_id":"fc_1","output_index":0,"arguments":"{}"}"#,
            )),
            Ok(wire_event(
                r#"{"type":"response.output_item.done","sequence_number":5,"output_index":0,
                    "item":{"id":"fc_1","type":"function_call","status":"completed",
                    "arguments":"{}","call_id":"call_abc","name":"get_marker"}}"#,
            )),
            Ok(wire_event(&codex_completed_frame(7))),
        ])
        .boxed();

        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let response = completed_response(&events);
        let calls: Vec<_> = assistant_items(response)
            .iter()
            .flat_map(|a| a.tool_calls.iter())
            .collect();
        assert_eq!(calls.len(), 1, "expected one tool call");
        assert_eq!(&*calls[0].id, "call_abc");
        assert_eq!(&*calls[0].name, "get_marker");
        assert_eq!(&*calls[0].arguments, "{}");
        assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
    }

    /// Terminal frame from the first-party xAI gateway: `output` carries the
    /// full completed array, as the Responses API contract promises. Modeled
    /// on the real xAI wire shape recorded in
    /// `xai_grok_test_support::sse::responses_api_events` (same `message` /
    /// `output_text` fixture used to script live xAI completions elsewhere
    /// in the test suite), not a struct literal, so deserialization
    /// differences between the wire and our types can't evade the guard.
    fn xai_completed_frame_with_message(text: &str, output_tokens: u32) -> String {
        format!(
            r#"{{"type":"response.completed","sequence_number":8,"response":{{
                "id":"resp_1","object":"response","created_at":1234567890,
                "model":"grok-build","status":"completed",
                "output":[{{"type":"message","id":"msg_1","role":"assistant",
                "status":"completed","content":[{{"type":"output_text",
                "text":"{text}","annotations":[]}}]}}],
                "usage":{{"input_tokens":10,"input_tokens_details":{{"cached_tokens":0}},
                "output_tokens":{output_tokens},
                "output_tokens_details":{{"reasoning_tokens":0}},"total_tokens":{total}}}
            }}}}"#,
            total = 10 + output_tokens,
        )
    }

    /// A terminal frame that *does* carry `output` stays authoritative — the
    /// per-item fallback must not duplicate or override it. This is the
    /// first-party xAI gateway shape.
    #[tokio::test]
    async fn completed_frame_with_output_wins_over_item_done_fallback() {
        let raw = stream::iter(vec![
            Ok(wire_event(
                r#"{"type":"response.output_item.done","sequence_number":7,"output_index":0,
                    "item":{"id":"msg_1","type":"message","status":"completed",
                    "content":[{"type":"output_text","annotations":[],"logprobs":[],
                    "text":"stale"}],"role":"assistant"}}"#,
            )),
            Ok(wire_event(&xai_completed_frame_with_message(
                "authoritative",
                5,
            ))),
        ])
        .boxed();

        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let response = completed_response(&events);
        let assistants = assistant_items(response);
        assert_eq!(assistants.len(), 1);
        assert_eq!(&*assistants[0].content, "authoritative");
    }

    #[test]
    fn empty_failed_response_is_not_treated_as_output() {
        let event = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        assert!(!responses_event_may_have_output(&event));
    }

    #[tokio::test]
    async fn response_failed_yields_failed_500() {
        let failed = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        let raw = stream::iter(vec![Ok(failed)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
                assert!(error.message.contains("boom"));
                // The wire code passes through verbatim — dropping it here
                // would disable strip recovery for coded Responses failures.
                assert_eq!(
                    error.error_code,
                    Some(xai_grok_sampling_types::ApiErrorCode::Other(
                        "server_error".into()
                    ))
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A coded `error` event must carry its code into the Failed info —
    /// this is the whole mid-stream strip-recovery chain for the Responses
    /// backend (the synthesized 500 + code classifies as an image error).
    #[tokio::test]
    async fn response_error_event_carries_code_into_failed() {
        let error_event = rs::ResponseStreamEvent::ResponseError(rs_types::ResponseErrorEvent {
            sequence_number: 0,
            code: Some(xai_grok_sampling_types::INVALID_IMAGE_ERROR_CODE.into()),
            message: "could not decode image".into(),
            param: None,
        });
        let raw = stream::iter(vec![Ok(error_event)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.error_code,
                    Some(xai_grok_sampling_types::ApiErrorCode::InvalidImage)
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mid_stream_transport_error_yields_failed() {
        let raw = stream::iter(vec![
            Ok(text_delta_event("hi")),
            Err(SamplingError::EventStreamError("conn reset".into())),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_when_stream_stalls() {
        let raw = stream::iter(vec![Ok(text_delta_event("hi"))])
            .chain(stream::pending())
            .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_millis(100),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
            }
            other => panic!("expected Failed(IdleTimeout), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_metadata_yielded_after_stream_started() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let metadata = ResponseModelMetadata {
            context_window: Some(8192),
            ..Default::default()
        };
        let events = collect(stream_responses(
            raw,
            Some(metadata),
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
        assert!(matches!(events[1], SamplingEvent::ModelMetadata { .. }));
    }

    #[test]
    fn meaningful_content_classifier_basics() {
        // Text delta with content is meaningful.
        let event = text_delta_event("foo");
        assert!(responses_event_has_meaningful_content(&event));
        // Empty text delta is not.
        let empty = text_delta_event("");
        assert!(!responses_event_has_meaningful_content(&empty));
        // Completed is meaningful (terminal).
        assert!(responses_event_has_meaningful_content(&completed_event()));
    }

    #[test]
    fn output_classifier_covers_non_forwarded_backend_events() {
        let queued = rs::ResponseStreamEvent::ResponseQueued(rs_types::ResponseQueuedEvent {
            sequence_number: 0,
            response: empty_completed_response(),
        });
        assert!(!responses_event_may_have_output(&queued));

        let response_error = rs::ResponseStreamEvent::ResponseError(rs_types::ResponseErrorEvent {
            sequence_number: 1,
            code: Some("server_error".into()),
            message: "failed before output".into(),
            param: None,
        });
        assert!(!responses_event_may_have_output(&response_error));

        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 1,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        assert!(responses_event_may_have_output(&refusal));

        let backend_progress = rs::ResponseStreamEvent::ResponseWebSearchCallSearching(
            rs_types::ResponseWebSearchCallSearchingEvent {
                sequence_number: 2,
                output_index: 0,
                item_id: "search-1".into(),
            },
        );
        assert!(responses_event_may_have_output(&backend_progress));
    }

    #[tokio::test]
    async fn tracked_stream_marks_non_forwarded_refusal_as_output() {
        let output_observed = Arc::new(AtomicBool::new(false));
        let refusal =
            rs::ResponseStreamEvent::ResponseRefusalDelta(rs_types::ResponseRefusalDeltaEvent {
                sequence_number: 0,
                item_id: "item-1".into(),
                output_index: 0,
                content_index: 0,
                delta: "no".into(),
            });
        let raw = stream::iter(vec![Ok(refusal), Ok(completed_event())]).boxed();
        let _ = collect(stream_responses_tracked(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            Arc::clone(&output_observed),
            FailedResponseCapture::default(),
        ))
        .await;

        assert!(output_observed.load(Ordering::Relaxed));
    }

    /// A server-side code-interpreter run surfaces as a generic backend tool
    /// call (started on InProgress, completed on OutputItemDone) named
    /// "code_interpreter" — the same shape as x_search — so it is no longer
    /// silently dropped from the event stream.
    #[tokio::test]
    async fn code_interpreter_forwards_backend_tool_call() {
        let in_progress = rs::ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(
            rs_types::ResponseCodeInterpreterCallInProgressEvent {
                sequence_number: 0,
                output_index: 0,
                item_id: "ci-1".into(),
            },
        );
        let done = rs::ResponseStreamEvent::ResponseOutputItemDone(
            rs_types::ResponseOutputItemDoneEvent {
                sequence_number: 1,
                output_index: 0,
                item: rs_types::OutputItem::CodeInterpreterCall(
                    rs_types::CodeInterpreterToolCall {
                        code: Some("print(1)".into()),
                        container_id: "cont-1".into(),
                        id: "ci-1".into(),
                        outputs: None,
                        status: rs_types::CodeInterpreterToolCallStatus::Completed,
                    },
                ),
            },
        );
        let raw = stream::iter(vec![Ok(in_progress), Ok(done), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                SamplingEvent::BackendToolCallStarted { call_id, name, .. }
                    if call_id == "ci-1" && name == "code_interpreter"
            )),
            "expected a code_interpreter BackendToolCallStarted, got {events:?}"
        );
        let completed = events.iter().find_map(|e| match e {
            SamplingEvent::BackendToolCallCompleted {
                call_id,
                name,
                result,
                ..
            } if name == "code_interpreter" => Some((call_id.clone(), result.clone())),
            _ => None,
        });
        let (call_id, result) = completed.expect("a code_interpreter BackendToolCallCompleted");
        assert_eq!(call_id, "ci-1");
        let result = result.expect("serialized code-interpreter payload");
        assert_eq!(result["code"], "print(1)");
    }

    fn function_call_added_event(
        output_index: u32,
        call_id: &str,
        name: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index,
            item: rs_types::OutputItem::FunctionCall(rs_types::FunctionToolCall {
                arguments: String::new(),
                call_id: call_id.into(),
                name: name.into(),
                id: None,
                status: None,
            }),
        })
    }

    fn function_call_args_delta_event(output_index: u32, delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
            rs_types::ResponseFunctionCallArgumentsDeltaEvent {
                sequence_number: 0,
                item_id: format!("item-{output_index}"),
                output_index,
                delta: delta.into(),
            },
        )
    }

    type Delta = (u32, Option<String>, Option<String>, Option<String>);

    /// Extract all ToolCallDelta events as (tool_index, id, name, arguments_delta).
    fn tool_call_deltas(evs: &[SamplingEvent]) -> Vec<Delta> {
        evs.iter()
            .filter_map(|e| match e {
                SamplingEvent::ToolCallDelta {
                    tool_index,
                    id,
                    name,
                    arguments_delta,
                    ..
                } => Some((
                    *tool_index,
                    id.clone(),
                    name.clone(),
                    arguments_delta.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn function_call_emits_initial_id_name_then_arg_deltas() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_xyz", "do_thing")),
            Ok(function_call_args_delta_event(0, "{\"x\":")),
            Ok(function_call_args_delta_event(0, "1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_xyz"));
        assert_eq!(deltas[0].2.as_deref(), Some("do_thing"));
        assert_eq!(deltas[0].3, None);
        assert_eq!(deltas[1].0, 0);
        assert_eq!(deltas[1].1, None);
        assert_eq!(deltas[1].2, None);
        assert_eq!(deltas[1].3.as_deref(), Some("{\"x\":"));
        assert_eq!(deltas[2].3.as_deref(), Some("1}"));
    }

    #[tokio::test]
    async fn function_call_args_delta_without_added_event_is_dropped() {
        // ArgumentsDelta with no preceding OutputItemAdded has no
        // output_index → tool_index mapping; drop silently.
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_args_delta_event(7, "{\"oops\":1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        assert_eq!(tool_call_deltas(&evs).len(), 0);
    }

    #[tokio::test]
    async fn multiple_function_calls_get_distinct_tool_indices() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_a", "tool_a")),
            Ok(function_call_added_event(1, "call_b", "tool_b")),
            Ok(function_call_args_delta_event(0, "a-args")),
            Ok(function_call_args_delta_event(1, "b-args")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_a"));
        assert_eq!(deltas[1].0, 1);
        assert_eq!(deltas[1].1.as_deref(), Some("call_b"));
        assert_eq!(deltas[2].0, 0);
        assert_eq!(deltas[2].3.as_deref(), Some("a-args"));
        assert_eq!(deltas[3].0, 1);
        assert_eq!(deltas[3].3.as_deref(), Some("b-args"));
    }

    #[tokio::test]
    async fn doom_loop_collector_signals_land_on_completed_response() {
        use xai_grok_sampling_types::doom_loop::{
            DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA,
        };
        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb(DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA));
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
                assert_eq!(
                    response.doom_loop_signals[0].raw,
                    "tail_repetition:4@response"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// An armed collector holding a confident signal aborts the attempt with
    /// a retryable doom-loop failure; disarmed, the same stream completes and
    /// the signals ride the response instead.
    #[tokio::test]
    async fn confident_signal_aborts_stream_unless_disarmed() {
        let confident = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#;

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.kind,
                    crate::events::SamplingErrorKind::DoomLoopDetected
                );
                assert!(error.is_retryable);
                assert_eq!(
                    error.doom_loop_triggers.as_deref(),
                    Some(&["tail_repetition:8@thinking".to_string()][..])
                );
            }
            other => panic!("expected Failed(DoomLoopDetected), got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        collector.disarm_abort();
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
            }
            other => panic!("expected Completed after disarm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn doom_loop_signals_empty_without_collector_or_triggers() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // A collector that never saw a trigger also leaves the field empty.
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(crate::doom_loop::DoomLoopSignalCollector::default()),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
