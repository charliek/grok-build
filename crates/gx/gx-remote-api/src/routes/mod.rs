//! Route table and the bearer-token gate.

pub mod approvals;
pub mod events;
pub mod health;
pub mod history;
pub mod messages;
pub mod sessions;

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Router, http};

use crate::error::ApiError;
use crate::state::AppState;

/// Every route the lane serves through C6.
///
/// `/v1/healthz` is registered on a separate router and merged in, so the token layer — attached
/// with `route_layer`, which only runs on a matched route — cannot reach it. That is the plan's
/// one exemption: a health probe has to work before a client has a token (and `gx remote status`
/// uses it to tell "bound" from "stale record").
pub fn router(state: Arc<AppState>) -> Router {
    let guarded = Router::new()
        .route(
            "/v1/sessions",
            get(sessions::list_sessions).post(messages::create_session),
        )
        .route("/v1/sessions/{id}", get(sessions::get_session))
        .route("/v1/sessions/{id}/history", get(history::get_history))
        .route("/v1/sessions/{id}/events", get(events::get_events))
        .route("/v1/sessions/{id}/messages", post(messages::post_message))
        .route("/v1/sessions/{id}/cancel", post(messages::post_cancel))
        .route(
            "/v1/sessions/{id}/approvals",
            get(approvals::list_approvals),
        )
        .route(
            "/v1/sessions/{id}/approvals/{tool_call_id}",
            get(approvals::get_approval).post(approvals::post_approval),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .route("/v1/healthz", get(health::healthz))
        .merge(guarded)
        .with_state(state)
}

/// `Authorization: Bearer <t>`, or `?token=<t>` for clients that cannot set a header.
///
/// The query form is a documented second choice — it lands in proxy logs and shell history — but
/// `EventSource` (C5's SSE clients) has no way to set headers, so it has to exist.
async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let presented = bearer_header(request.headers())
        .map(str::to_string)
        .or_else(|| query_token(request.uri().query()));

    match presented {
        Some(token) if state.token.matches(&token) => Ok(next.run(request).await),
        // Same answer for absent and wrong: nothing here should help someone guess.
        _ => Err(ApiError::Unauthorized),
    }
}

/// Parse a request body into `T`, reporting a failure in *this* API's error envelope.
///
/// Deliberately not axum's `Json<T>` extractor: its rejection is axum's own shape, so a phone with
/// a typo in its body would get an error it has no parser for, on the one code path where a clear
/// message matters most.
pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|err| ApiError::BadRequest(format!("could not parse the request body: {err}")))
}

/// The token out of an `Authorization: Bearer …` header, if it is well formed.
fn bearer_header(headers: &http::HeaderMap) -> Option<&str> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    // RFC 7235: the scheme is case-insensitive.
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// The `token` query parameter, if present.
///
/// Parsed by hand rather than through `serde_urlencoded`: the token is hex, so there is nothing to
/// percent-decode, and a hand parse cannot reject the whole request over an unrelated parameter.
fn query_token(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(key, _)| *key == "token")
            .map(|(_, value)| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        map.insert(http::header::AUTHORIZATION, value.parse().unwrap());
        map
    }

    #[test]
    fn bearer_header_accepts_any_case_of_the_scheme() {
        assert_eq!(bearer_header(&headers("Bearer abc")), Some("abc"));
        assert_eq!(bearer_header(&headers("bearer abc")), Some("abc"));
        assert_eq!(bearer_header(&headers("BEARER abc")), Some("abc"));
    }

    #[test]
    fn bearer_header_rejects_other_schemes_and_empty_values() {
        assert_eq!(bearer_header(&headers("Basic abc")), None);
        assert_eq!(bearer_header(&headers("abc")), None);
        assert_eq!(bearer_header(&headers("Bearer ")), None);
        assert_eq!(bearer_header(&http::HeaderMap::new()), None);
    }

    #[test]
    fn query_token_finds_the_parameter_anywhere_in_the_string() {
        assert_eq!(query_token(Some("token=abc")), Some("abc".into()));
        assert_eq!(
            query_token(Some("offset=0&token=abc&limit=5")),
            Some("abc".into())
        );
        assert_eq!(query_token(Some("offset=0")), None);
        assert_eq!(query_token(None), None);
        // A parameter that merely ends in "token" is not the token.
        assert_eq!(query_token(Some("mytoken=abc")), None);
    }
}
