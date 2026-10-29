//! The HTTPS router the kit's `webhook_server` serves on every replica
//! (`WEBHOOK_PORT`, default 8443): the probes — `/readyz`, `/livez`,
//! `/healthz` — and `/scope`, the kit's `ScopeGuard` (the chart's
//! `scope.<group>` webhook, rendered with a `watch.labelSelector`).
//!
//! TODO(2d): `POST /validate`, the admission webhook
//! (`weebo_kit_runtime::admission::router`): each CR parsed and checked
//! by Notify's own model (`notify-config`), as the reconcilers do.

use std::sync::Arc;

use axum::Router;
use domain::error::ReasonCode;
use weebo_kit_runtime::admission::{ScopeGuard, scope_router};
use weebo_kit_runtime::{Health, WatchScope};

use crate::controller::FINALIZER;

pub fn router(health: Arc<Health>, scope: WatchScope) -> Router {
    let guard = ScopeGuard {
        scope,
        finalizer: FINALIZER,
        annotations: api::ANNOTATIONS,
        reason: ReasonCode::LeavesWatchScope,
    };
    weebo_kit_runtime::health::routes(health).merge(scope_router(Arc::new(guard)))
}
