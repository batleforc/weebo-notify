//! One `reconcile_*` use-case per role: the instance, and the objects bound
//! to it (one generic use-case for the four kinds). Each returns an
//! [`Outcome`] the controller turns into a status patch and Events —
//! always through a `ReasonCode`, never a free string.

pub mod reconcile_instance;
pub mod reconcile_object;

use domain::error::ReasonCode;
use domain::{Advisory, Failure};

use crate::ports::{
    ApiError, BoundInstance, CredentialError, InstanceError, WorkloadError, instance_reason,
    workload_reason,
};

/// An instance that couldn't be resolved, as a failure (`Failure` and
/// `InstanceError` both being the kit's, no `From` between them).
pub fn instance_failure(err: InstanceError) -> Failure {
    Failure::new(instance_reason(&err), err.to_string())
}

/// Owned objects that couldn't be written or read, as a failure.
pub fn workload_failure(err: WorkloadError) -> Failure {
    Failure::new(workload_reason(&err), err.to_string())
}

/// A port's Kubernetes API call that failed, as a failure.
pub fn api_failure(err: ApiError) -> Failure {
    Failure::new(err.reason(), err.to_string())
}

/// A Secret that can't be used, as a failure.
pub fn credential_failure(err: CredentialError) -> Failure {
    Failure::new(err.reason(), err.to_string())
}

/// What a reconcile ended on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The instance the CR was resolved to, to record in its status.
    pub instance: Option<BoundInstance>,
    /// `Ok(message)` for `Ready: True`.
    pub ready: Result<String, Failure>,
    /// Non-blocking conditions shown next to `Ready`.
    pub advisories: Vec<Advisory>,
}

impl Outcome {
    pub fn synced(message: impl Into<String>) -> Self {
        Self {
            instance: None,
            ready: Ok(message.into()),
            advisories: Vec::new(),
        }
    }

    pub fn failed(failure: impl Into<Failure>) -> Self {
        Self {
            instance: None,
            ready: Err(failure.into()),
            advisories: Vec::new(),
        }
    }

    /// Records the instance the CR is bound to.
    pub fn on(mut self, instance: BoundInstance) -> Self {
        self.instance = Some(instance);
        self
    }

    /// The reason `Ready` ends on — what metrics and Events are keyed by.
    pub fn reason(&self) -> ReasonCode {
        match &self.ready {
            Ok(_) => ReasonCode::Reconciled,
            Err(f) => f.reason,
        }
    }
}
