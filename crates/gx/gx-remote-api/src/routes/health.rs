//! `GET /v1/healthz` — the one unauthenticated route.
//!
//! It answers three questions a discovery reader has before it has a token: is this port still the
//! lane (vs. a stale record pointing at whatever bound the port next), which leader is it, and is
//! it a gx build. `build: "gx"` is a constant, not a probe: a stock-flavoured binary never starts
//! the lane at all (`is_gx_build()` gate, C7).

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub ok: bool,
    pub version: String,
    pub leader_pid: u32,
    pub instance_id: String,
    pub build: &'static str,
}

pub async fn healthz(State(state): State<Arc<AppState>>) -> Json<Health> {
    Json(Health {
        ok: true,
        version: state.health.version.clone(),
        leader_pid: state.health.leader_pid,
        instance_id: state.health.instance_id.clone(),
        build: "gx",
    })
}
