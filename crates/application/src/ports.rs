//! Port traits: what the use-cases need from the outside world. Which
//! instance a CR lives on is the kit's [`InstanceDirectory`] (on the local
//! instances only); the objects of an instance are the kit's
//! [`OwnedObjects`].

use api::local_instance::ObjectSelector;
use domain::error::ReasonCode;
pub use weebo_kit_application::instances::{
    BoundInstance, InstanceDirectory, InstanceError, ResolveError,
};
pub use weebo_kit_application::workload::{OwnedObject, OwnedObjects, Owner, WorkloadError};

use crate::objects::ObjectView;

/// The reason code of an instance that couldn't be resolved.
pub fn instance_reason(err: &InstanceError) -> ReasonCode {
    match err {
        InstanceError::Kubernetes(_) => ReasonCode::KubernetesApiError,
        InstanceError::Resolve(e) => match e {
            ResolveError::NotFound { .. } | ResolveError::NoInstance => {
                ReasonCode::InstanceNotFound
            }
            ResolveError::Ambiguous { .. } => ReasonCode::InstanceAmbiguous,
            ResolveError::InvalidRef(_) => ReasonCode::InvalidSpec,
            ResolveError::OutsideScope(_) | ResolveError::NotShared { .. } => {
                ReasonCode::InstanceNotAllowed
            }
            ResolveError::BindingChanged { .. } => ReasonCode::InstanceBindingChanged,
        },
    }
}

/// The reason code of a [`WorkloadError`].
pub fn workload_reason(e: &WorkloadError) -> ReasonCode {
    match e {
        WorkloadError::Conflict(_) => ReasonCode::OwnedObjectConflict,
        WorkloadError::Kubernetes(_) => ReasonCode::KubernetesApiError,
        WorkloadError::Refused(_) => ReasonCode::InvalidSpec,
        WorkloadError::SecretNotAllowed(_) => ReasonCode::SecretNotAllowed,
    }
}

/// Why a Secret a spec references can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// The Secret or its key is missing.
    #[error("{0}")]
    Missing(String),
    /// The Secret lacks `notify.weebo.io/credential: "true"`.
    #[error("{0}")]
    NotAllowed(String),
    #[error("Kubernetes API error: {0}")]
    Kubernetes(String),
}

impl CredentialError {
    pub fn reason(&self) -> ReasonCode {
        match self {
            CredentialError::Missing(_) => ReasonCode::SecretMissing,
            CredentialError::NotAllowed(_) => ReasonCode::SecretNotAllowed,
            CredentialError::Kubernetes(_) => ReasonCode::KubernetesApiError,
        }
    }
}

/// A Kubernetes API call of a port that failed: what was asked, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The API server refused the operator (401/403): its RBAC, which a
    /// retry doesn't fix.
    #[error("permission denied: {0}")]
    Denied(String),
    /// What the call was built from doesn't read (an invalid label
    /// selector of the spec).
    #[error("{0}")]
    Invalid(String),
    /// Unreachable, overloaded, timing out, or another refusal: usually
    /// transient.
    #[error("Kubernetes API error: {0}")]
    Failed(String),
}

impl ApiError {
    pub fn reason(&self) -> ReasonCode {
        match self {
            ApiError::Denied(_) | ApiError::Failed(_) => ReasonCode::KubernetesApiError,
            ApiError::Invalid(_) => ReasonCode::InvalidSpec,
        }
    }
}

/// The Secrets the specs reference, used only when they carry the
/// credential label. The pods read their values (`WNK_*`, mounted files):
/// the configuration never holds one.
#[async_trait::async_trait]
pub trait Credentials: Send + Sync {
    /// Whether the Secret `name` of `namespace` exists, consents and has
    /// `key` — its value never read out.
    async fn check(&self, namespace: &str, name: &str, key: &str) -> Result<(), CredentialError>;
    /// The UTF-8 value of `key` of the Secret `name` of `namespace` (a
    /// sink's secret, copied next to the instance's pods).
    async fn read(&self, namespace: &str, name: &str, key: &str)
    -> Result<String, CredentialError>;
}

/// Fresh secrets the operator generates.
pub trait KeyGenerator: Send + Sync {
    /// A key Notify takes as a secret string (the sinks' fingerprint key,
    /// the filesystem store's link key): 48 random bytes, base64 (64
    /// characters, over Notify's 32-byte floor).
    fn key(&self) -> String;
    /// An Ed25519 private key, PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`),
    /// as `openssl genpkey -algorithm ed25519` writes it: the federation
    /// signing key.
    fn ed25519_pem(&self) -> String;
}

/// Where a `Job` stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobState {
    pub active: bool,
    /// Its `Complete` condition.
    pub succeeded: bool,
    /// Its `Failed` condition, and why.
    pub failed: Option<String>,
}

/// The instances' migration `Job`s.
#[async_trait::async_trait]
pub trait Jobs: Send + Sync {
    /// The `Job` `name` of `namespace`, `None` when absent.
    async fn job(&self, namespace: &str, name: &str) -> Result<Option<JobState>, ApiError>;
}

/// Where a reconcile records status fields right after the write they
/// describe (a status merge patch): a pass that fails half-way must not
/// forget it wrote.
#[async_trait::async_trait]
pub trait StatusLedger: Send + Sync {
    async fn record(
        &self,
        namespace: &str,
        name: &str,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), ApiError>;
}

/// The `NotifyChannel`s, `NotifyServiceAccount`s, `NotifyTransform`s and
/// `NotifyTrustPolicy`s bound to an instance.
#[async_trait::async_trait]
pub trait ObjectDirectory: Send + Sync {
    /// Those recording `instance` in `status.instance` (their own
    /// controller binds them), in scope of the operator, matching its
    /// `selector`, from its own namespace or one it accepts — in any
    /// order: the render sorts them.
    async fn bound_to(
        &self,
        instance: &Owner,
        selector: &ObjectSelector,
    ) -> Result<Vec<ObjectView>, ApiError>;
}

/// The instances an object may bind to: resolution through the instance's
/// consent (`objectSelector.namespaces`), and the instance itself.
#[async_trait::async_trait]
pub trait AttachedInstances: Send + Sync {
    async fn resolve_attached(
        &self,
        namespace: &str,
        instance_ref: Option<&weebo_kit_api::InstanceRef>,
        recorded: Option<&BoundInstance>,
    ) -> Result<BoundInstance, InstanceError>;
    async fn instance(&self, bound: &BoundInstance)
    -> Result<Option<api::LocalInstance>, ApiError>;
}
