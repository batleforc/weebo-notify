//! One `kube::runtime::Controller` per CRD, on the kit's skeleton
//! (triggers, concurrency, requeue, finalizer, Events, metrics). The
//! reconcile bodies delegate to `application::use_cases`.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use application::ports::{
    AttachedInstances, Credentials, InstanceDirectory, Jobs, KeyGenerator, KeyIssuer,
    ObjectDirectory, OwnedObjects, ReplicaHealth, SecretStores, ServiceAccounts, StatusLedger,
};
use application::use_cases::Outcome;
use domain::Failure;
use domain::error::ReasonCode;
use kube::runtime::controller::Action;
use kube::runtime::events::Recorder;
use kube::{Client, Resource};
use weebo_kit_runtime::events;
use weebo_kit_runtime::metrics::ReconcileMetrics;
use weebo_kit_runtime::shared::SharedWatches;
use weebo_kit_runtime::{Health, WatchScope};

/// Resolves once the process is asked to stop: every controller then stops
/// taking new work, lets its in-flight reconciles finish, and its `run`
/// returns.
pub use weebo_kit_runtime::Shutdown;
/// The kit's controller skeleton: a watch filtered to what reconciling
/// acts on (never the CR's own status patches), bounded concurrency
/// (`WEEBO_RECONCILE_CONCURRENCY`), the result stream drained into the
/// liveness.
pub use weebo_kit_runtime::controller::{controller, drive};

pub mod api_key;
pub mod local_instance;
pub mod object;

/// Field manager of the status patches, meter and Event reporter name.
pub const FIELD_MANAGER: &str = "weebo-notify-operator";
pub const FINALIZER: &str = "notify.weebo.io/finalizer";
/// Prefix of every metric: `<prefix>_reconcile_total`, `<prefix>_leader`…
pub const METRIC_PREFIX: &str = "weebo_notify";

/// Shared across every controller. Concrete adapters are built and
/// injected once, in `operator::main` (tests inject fakes).
pub struct Ctx {
    pub client: Client,
    /// Which instance a CR lives on (the kit's `LocalInstances`, on the
    /// watch scope).
    pub instances: Arc<dyn InstanceDirectory>,
    /// The instances an object may bind to.
    pub attached: Arc<dyn AttachedInstances>,
    /// The objects of the instances (the kit's `KubeOwnedObjects`).
    pub objects: Arc<dyn OwnedObjects>,
    /// The Secrets the specs reference (credential label required).
    pub credentials: Arc<dyn Credentials>,
    pub keys: Arc<dyn KeyGenerator>,
    /// The channels, service accounts, transforms and trust policies bound
    /// to an instance.
    pub directory: Arc<dyn ObjectDirectory>,
    /// Where an instance's pass records what it just wrote.
    pub ledger: Arc<dyn StatusLedger>,
    /// The migration `Job`s.
    pub jobs: Arc<dyn Jobs>,
    /// The instances' server pods' `/healthz`.
    pub health_checks: Arc<dyn ReplicaHealth>,
    /// The service accounts API keys name.
    pub accounts: Arc<dyn ServiceAccounts>,
    /// Notify's key routes, as an instance's key issuer.
    pub issuer: Arc<dyn KeyIssuer>,
    /// Where API keys are written (Kubernetes, the operator's Vault).
    pub secret_stores: Arc<dyn SecretStores>,
    /// The operator's own pods, let in by an instance's `NetworkPolicy`.
    pub operator: weebo_kit_application::workload::OperatorPod,
    /// The operator's own settings.
    pub settings: Settings,
    /// One recorder for the whole process: the kit folds repeated Events
    /// into a series only within a recorder.
    pub recorder: Recorder,
    pub shutdown: Shutdown,
    /// Liveness: controllers report their watch errors here.
    pub health: Arc<Health>,
    /// What the controllers watch (`WEEBO_WATCH_NAMESPACE`,
    /// `WEEBO_WATCH_LABEL_SELECTOR`): every watch goes through it.
    pub scope: WatchScope,
    /// The binary's shared watches ([`spawn_watches`]): one per CR kind
    /// for every controller and every store read
    /// (`SharedWatches::default()`: each controller watches on its own).
    pub watches: SharedWatches,
}

/// One watch per CR kind, on `scope`, for every controller (as its own
/// kind or a referenced one) and the stores read in place of a LIST.
/// Spawned before the lease is campaigned for: a standby keeps them warm.
pub fn spawn_watches(client: &Client, scope: &WatchScope, health: &Arc<Health>) -> SharedWatches {
    let mut watches = SharedWatches::default();
    watches.spawn(
        scope.api::<api::LocalInstance>(client.clone()),
        scope,
        health,
        "NotifyLocalInstance watch",
    );
    watches.spawn(
        scope.api::<api::Channel>(client.clone()),
        scope,
        health,
        "NotifyChannel watch",
    );
    watches.spawn(
        scope.api::<api::ServiceAccount>(client.clone()),
        scope,
        health,
        "NotifyServiceAccount watch",
    );
    watches.spawn(
        scope.api::<api::Transform>(client.clone()),
        scope,
        health,
        "NotifyTransform watch",
    );
    watches.spawn(
        scope.api::<api::TrustPolicy>(client.clone()),
        scope,
        health,
        "NotifyTrustPolicy watch",
    );
    watches.spawn(
        scope.api::<api::ApiKey>(client.clone()),
        scope,
        health,
        "NotifyApiKey watch",
    );
    watches
}

/// What the operator's environment sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `WEEBO_NOTIFY_DEFAULT_IMAGE`: the server's image of an instance
    /// without one.
    pub default_image: String,
    /// The web UI's image of an instance without one.
    pub default_web_image: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_image: format!("{}:{}", api::notify::SERVER_IMAGE, api::notify::VERSION),
            default_web_image: format!("{}:{}", api::notify::WEB_IMAGE, api::notify::VERSION),
        }
    }
}

/// What stopped a reconcile before (or instead of) its status.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),
    /// Anything the finalizer helper reports: the reconcile's own error
    /// (`ApplyFailed`/`CleanupFailed`) or adding/removing the finalizer.
    #[error("{0}")]
    Finalizer(Box<kube::runtime::finalizer::Error<Error>>),
    /// A cleanup that can't finish yet: the finalizer stays until it can
    /// (or the CR is force-orphaned).
    #[error("{0}")]
    Cleanup(Failure),
}

impl Error {
    /// The reason code of the Event announcing this error, and whether it
    /// happened reconciling or cleaning up (the Event's `action`).
    pub fn reason(&self) -> (ReasonCode, &'static str) {
        use kube::runtime::finalizer::Error as Finalizer;
        match self {
            Error::Kube(_) => (ReasonCode::KubernetesApiError, events::RECONCILE),
            Error::Cleanup(failure) => (failure.reason, events::CLEANUP),
            Error::Finalizer(err) => match err.as_ref() {
                Finalizer::ApplyFailed(inner) => inner.reason(),
                Finalizer::CleanupFailed(inner) => (inner.reason().0, events::CLEANUP),
                _ => (ReasonCode::KubernetesApiError, events::RECONCILE),
            },
        }
    }
}

/// Every reconcile that errors out is announced by a `Warning` Event on
/// its CR, then retried in ~30s, backing off while it keeps failing (the
/// kit's). A cleanup waiting on its instance is read again soon, as
/// [`requeue_after`] does: it is a wait, not a failure.
pub fn error_policy<K>(obj: Arc<K>, err: &Error, ctx: Arc<Ctx>) -> Action
where
    K: Resource<DynamicType = ()>,
{
    let (reason, action) = err.reason();
    let retry = weebo_kit_runtime::controller::retry_after_error(
        &ctx.recorder,
        obj.object_ref(&()),
        reason,
        action,
        err,
    );
    if matches!(
        reason,
        ReasonCode::ConfigPropagating | ReasonCode::RemovalRefused | ReasonCode::InstanceInUse
    ) {
        Action::requeue(Duration::from_secs(15))
    } else {
        retry
    }
}

/// Steady-state drift check after a success, a quick retry after a
/// failure, backing off while `obj` keeps failing (jittered — the kit's).
pub fn requeue_after<K: Resource>(obj: &K, outcome: &Outcome) -> Action {
    match &outcome.ready {
        // A configuration being confirmed, a migration running, an API key
        // waiting for its account or its instance's key issuer: read again
        // soon (the watches are the fast path).
        Err(f)
            if matches!(
                f.reason,
                ReasonCode::ConfigPropagating
                    | ReasonCode::Migrating
                    | ReasonCode::ServiceAccountNotReady
                    | ReasonCode::KeyIssuerUnavailable
            ) =>
        {
            weebo_kit_runtime::controller::requeue_after(obj, false, Some(15))
        }
        // A rollout's progress comes through the Deployment's watch: the
        // steady interval is the fallback.
        Err(f) if f.reason == ReasonCode::WorkloadNotReady => {
            weebo_kit_runtime::controller::requeue_after(obj, true, None)
        }
        Err(f) => {
            weebo_kit_runtime::controller::requeue_after(obj, f.reason.needs_a_change(), None)
        }
        Ok(_) => weebo_kit_runtime::controller::requeue_after(obj, true, None),
    }
}

/// [`requeue_after`] for what waits on its instance (the objects): the
/// instance's status changes wake it through the watch, the steady
/// interval is the fallback.
pub fn requeue_waiting<K: Resource>(obj: &K, outcome: &Outcome) -> Action {
    match &outcome.ready {
        Err(f) if f.reason == ReasonCode::ConfigPropagating => {
            weebo_kit_runtime::controller::requeue_after(obj, true, None)
        }
        _ => requeue_after(obj, outcome),
    }
}

/// A hash of `value`'s JSON: what a watch predicate compares.
pub fn hash_of<T: serde::Serialize>(value: &T) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(value).ok()?.hash(&mut hasher);
    Some(hasher.finish())
}

/// `<prefix>_reconcile_total` / `_duration_seconds` (the kit's
/// `ReconcileMetrics`). A no-op until the telemetry installs a real
/// `MeterProvider`.
pub fn record_reconcile(kind: &'static str, started: Instant, outcome: &Outcome) {
    static METRICS: OnceLock<ReconcileMetrics> = OnceLock::new();
    METRICS
        .get_or_init(|| ReconcileMetrics::new(FIELD_MANAGER, METRIC_PREFIX))
        .record(kind, outcome.reason(), started.elapsed());
}

/// A finalizer's cleanup — unless the CR carries
/// `notify.weebo.io/force-orphan: "true"`, the way out for a CR whose
/// cleanup can't succeed any more — then a `Released` Event (the kit's).
pub async fn finalize<K>(
    obj: &K,
    ctx: &Ctx,
    cleanup: impl Future<Output = Result<(), Error>>,
) -> Result<Action, Error>
where
    K: Resource<DynamicType = ()>,
{
    weebo_kit_runtime::controller::finalize(
        obj,
        &ctx.recorder,
        &api::ANNOTATIONS,
        ReasonCode::Released,
        cleanup,
    )
    .await
}

#[cfg(test)]
mod tests {
    use kube::runtime::finalizer::Error as Finalizer;

    use super::*;

    #[test]
    fn errors_map_to_a_reason_and_an_action() {
        let waiting = || Error::Cleanup(Failure::new(ReasonCode::ConfigPropagating, "x"));
        assert_eq!(
            waiting().reason(),
            (ReasonCode::ConfigPropagating, events::CLEANUP)
        );
        assert_eq!(
            Error::Finalizer(Box::new(Finalizer::CleanupFailed(waiting()))).reason(),
            (ReasonCode::ConfigPropagating, events::CLEANUP)
        );
        assert_eq!(
            Error::Finalizer(Box::new(Finalizer::UnnamedObject)).reason(),
            (ReasonCode::KubernetesApiError, events::RECONCILE)
        );
    }

    #[test]
    fn the_default_images_are_the_targeted_version() {
        let settings = Settings::default();
        assert_eq!(
            settings.default_image,
            format!("ghcr.io/batleforc/weebo-notify-v2:{}", api::notify::VERSION)
        );
        assert!(settings.default_web_image.contains("weebo-notify-v2-web:"));
    }
}
