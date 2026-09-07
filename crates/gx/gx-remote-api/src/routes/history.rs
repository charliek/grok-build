//! `GET /v1/sessions/{id}/history` — the persisted transcript, paginated.
//!
//! This is the first route that touches a session's content, so it is the first that attaches: the
//! handler resolves the session (for its `cwd`), sends `session/load` and **awaits** it, and only
//! then asks for updates. The order matters — `x.ai/session/updates` reads the session's
//! `updates.jsonl`, and a session the leader has not loaded may not have flushed its tail yet.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use serde_json::{Value, json};

use crate::envelope::NormalizedEnvelope;
use crate::error::ApiError;
use crate::routes::sessions::{ensure_attached, resolve_session};
use crate::state::AppState;

const SESSION_UPDATES: &str = "x.ai/session/updates";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    pub updates: Vec<NormalizedEnvelope>,
    pub total_count: u64,
    pub has_more: bool,
    /// The newest `eventId` in the *whole* session, not just this page; `null` when no line in the
    /// page carried one. C5 resumes an SSE stream from it.
    pub last_event_id: Option<String>,
}

/// `?offset=&limit=`. A negative `offset` counts back from the end, which the leader supports and
/// this route passes straight through.
#[derive(Debug, Default, PartialEq)]
struct HistoryQuery {
    offset: Option<i64>,
    limit: Option<u64>,
}

pub async fn get_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<History>, ApiError> {
    let query = HistoryQuery::parse(query.as_deref())?;

    // 404s before anything is sent to the leader if the session does not exist at all.
    let session = resolve_session(&state, &id).await?;
    ensure_attached(&state, &id, &session.cwd).await?;

    let mut params = json!({ "sessionId": id, "cwd": session.cwd });
    if let Some(offset) = query.offset {
        params["offset"] = json!(offset);
    }
    if let Some(limit) = query.limit {
        params["limit"] = json!(limit);
    }

    let response = state.acp.request(SESSION_UPDATES, params).await?;
    Ok(Json(normalize_response(&response)))
}

/// Turn the leader's `{ updates, totalCount, hasMore, lastEventId }` into ours.
fn normalize_response(response: &Value) -> History {
    let updates = response
        .get("updates")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(NormalizedEnvelope::from_stored).collect())
        .unwrap_or_default();

    History {
        updates,
        total_count: response
            .get("totalCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        has_more: response
            .get("hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        last_event_id: response
            .get("lastEventId")
            .and_then(Value::as_str)
            .map(String::from),
    }
}

impl HistoryQuery {
    /// Parse `offset` and `limit` out of a raw query string.
    ///
    /// Hand-parsed for the same reason as the token (see `super::query_token`): both values are
    /// integers, so there is nothing to percent-decode, and an unrelated parameter — `token=…`,
    /// which every header-less client sends — must not make the whole request a 400.
    fn parse(query: Option<&str>) -> Result<Self, ApiError> {
        let mut parsed = Self::default();
        let Some(query) = query else {
            return Ok(parsed);
        };
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "offset" => {
                    parsed.offset = Some(value.parse().map_err(|_| {
                        ApiError::BadRequest(format!("offset must be an integer, got {value:?}"))
                    })?);
                }
                "limit" => {
                    parsed.limit = Some(value.parse().map_err(|_| {
                        ApiError::BadRequest(format!(
                            "limit must be a non-negative integer, got {value:?}"
                        ))
                    })?);
                }
                _ => {}
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing_takes_offset_and_limit_and_ignores_the_rest() {
        assert_eq!(HistoryQuery::parse(None).unwrap(), HistoryQuery::default());
        assert_eq!(
            HistoryQuery::parse(Some("token=deadbeef")).unwrap(),
            HistoryQuery::default()
        );
        assert_eq!(
            HistoryQuery::parse(Some("offset=-100&limit=50&token=x")).unwrap(),
            HistoryQuery {
                offset: Some(-100),
                limit: Some(50)
            }
        );
    }

    #[test]
    fn a_non_numeric_bound_is_a_bad_request() {
        let err = HistoryQuery::parse(Some("limit=lots")).unwrap_err();
        assert_eq!(err.code(), "bad_request");
        let err = HistoryQuery::parse(Some("offset=soon")).unwrap_err();
        assert_eq!(err.code(), "bad_request");
        // A negative limit is not a tail request; only offset counts backwards.
        assert_eq!(
            HistoryQuery::parse(Some("limit=-1")).unwrap_err().code(),
            "bad_request"
        );
    }

    #[test]
    fn a_response_normalizes_every_row_and_carries_the_pagination_fields() {
        let response = json!({
            "updates": [
                { "timestamp": 1, "method": "session/update", "params": { "_meta": { "eventId": "s-1" } } },
                { "timestamp": 2, "method": "session/update", "params": {} }
            ],
            "totalCount": 12,
            "hasMore": true,
            "lastEventId": "s-1"
        });
        let history = normalize_response(&response);
        assert_eq!(history.total_count, 12);
        assert!(history.has_more);
        assert_eq!(history.last_event_id.as_deref(), Some("s-1"));
        assert_eq!(history.updates.len(), 2);
        assert_eq!(history.updates[0].event_id.as_deref(), Some("s-1"));
        assert_eq!(history.updates[1].event_id, None);
    }

    #[test]
    fn a_response_missing_the_optional_fields_still_normalizes() {
        // `lastEventId` is omitted entirely when no line in the page had one.
        let history =
            normalize_response(&json!({ "updates": [], "totalCount": 0, "hasMore": false }));
        assert_eq!(history.last_event_id, None);
        assert!(history.updates.is_empty());
    }
}
