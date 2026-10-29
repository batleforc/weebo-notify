//! The HTTPS router the kit's `webhook_server` serves on every replica
//! (`WEBHOOK_PORT`, default 8443): the probes — `/readyz`, `/livez`,
//! `/healthz` —, `/validate`, the admission webhook
//! ([`crate::admission`]), and `/scope`, the kit's `ScopeGuard` (the
//! chart's `scope.<group>` webhook, rendered with a
//! `watch.labelSelector`).

use std::sync::Arc;

use axum::Router;
use domain::error::ReasonCode;
use weebo_kit_runtime::admission::{ScopeGuard, scope_router};
use weebo_kit_runtime::{Health, WatchScope};

use crate::admission::NotifyAdmission;
use crate::controller::FINALIZER;

pub fn router(health: Arc<Health>, scope: WatchScope, admission: Arc<NotifyAdmission>) -> Router {
    let guard = ScopeGuard {
        scope,
        finalizer: FINALIZER,
        annotations: api::ANNOTATIONS,
        reason: ReasonCode::LeavesWatchScope,
    };
    crate::admission::router(admission, health).merge(scope_router(Arc::new(guard)))
}
