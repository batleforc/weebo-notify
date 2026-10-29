//! Outbound adapters. Notify has no administration API the operator could
//! drive: it talks to it through the file it renders and the objects it
//! owns (the kit's `KubeOwnedObjects`). What is the operator's own here:
//! which instance a CR lives on ([`LocalInstanceDirectory`]), the objects
//! bound to an instance ([`KubeObjects`]), the Secrets the specs name
//! ([`KubeCredentials`]), fresh keys ([`RandomKeys`]), the instance's status
//! records ([`KubeInstanceLedger`]) and the settings
//! tying the kit's adapters to `NotifyLocalInstance`
//! ([`workload_settings`]).

pub mod cluster;
pub mod instances;
pub mod jobs;
pub mod ledger;
pub mod objects;

use application::ports::ApiError;
pub use cluster::{KubeCredentials, RandomKeys};
pub use instances::LocalInstanceDirectory;
pub use jobs::KubeJobs;
pub use ledger::KubeInstanceLedger;
pub use objects::KubeObjects;
use weebo_kit_adapters::WorkloadSettings;

/// A failed Kubernetes call as the ports' [`ApiError`]: `what` was being
/// done, a 401/403 (the operator's RBAC) kept apart from the rest.
pub fn api_error(what: impl std::fmt::Display) -> impl FnOnce(kube::Error) -> ApiError {
    move |e| {
        let message = format!("{what}: {e}");
        match &e {
            kube::Error::Api(s) if s.code == 401 || s.code == 403 => ApiError::Denied(message),
            _ => ApiError::Failed(message),
        }
    }
}

/// The field manager of every apply and status patch.
pub const FIELD_MANAGER: &str = "weebo-notify-operator";

/// What ties the kit's `KubeOwnedObjects` to the operator: its field
/// manager, its `managed-by`, `NotifyLocalInstance` as the owner, the
/// credential label and the images a pod may run.
pub fn workload_settings(allowed_images: Vec<String>) -> WorkloadSettings {
    WorkloadSettings {
        field_manager: FIELD_MANAGER.into(),
        managed_by: api::MANAGED_BY.into(),
        owner_api_version: format!("{}/{}", api::GROUP, api::VERSION),
        owner_kind: "NotifyLocalInstance".into(),
        credential_label: api::CREDENTIAL_LABEL.into(),
        allowed_images,
    }
}
