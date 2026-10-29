//! The statuses of the operator's CRDs.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub use weebo_kit_api::{BoundInstance, Condition};

/// The status of a channel, service account, transform or trust policy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectStatus {
    /// The `metadata.generation` the last successful reconcile applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// The instance this object was resolved to and stays bound to: a
    /// `spec.instanceRef` by name only never silently moves to another
    /// instance created later under the same name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<BoundInstance>,
    /// The id it has in Notify (`spec.id`, or `metadata.name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `Ready` (in a configuration every ready pod of the instance applied),
    /// carrying one of the operator's reason codes.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// The status of a `NotifyLocalInstance`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceStatus {
    /// The `metadata.generation` the last successful reconcile applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// `Ready`, `Migrated`, carrying one of the operator's reason codes.
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// The API's URL inside the cluster (its `Service`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The Notify tag the pods run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The tag the database schema was last migrated to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_replicas: Option<i32>,
    /// The hash of what the server reads at boot only; a new one rolls the
    /// pods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_hash: Option<String>,
    /// The SHA-256 of the `config.toml` last written to the `ConfigMap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
    /// The last `configDigest` every ready pod reported applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_config_digest: Option<String>,
    /// The removals Notify's guard (`reconcile.maxRemovals`) refused in its
    /// last pass: their objects keep their finalizer.
    #[serde(default, skip_serializing_if = "RefusedRemovals::is_empty")]
    pub refused_removals: RefusedRemovals,
    /// The channels, service accounts, transforms and trust policies bound
    /// to the instance and what became of each (`<kind>/<namespace>/<name>`,
    /// [`object_key`]): rendered, or why not.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub objects: BTreeMap<String, ObjectRender>,
}

/// The key of an object in [`InstanceStatus::objects`]:
/// `<kind>/<namespace>/<name>` (`NotifyChannel/team/alerts`).
pub fn object_key(kind: &str, namespace: &str, name: &str) -> String {
    format!("{kind}/{namespace}/{name}")
}

/// What an instance made of an object bound to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRender {
    /// Its id in Notify (none for a trust policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub state: RenderState,
    /// Why it isn't rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The object holding its id (`namespace/name`), in conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_with: Option<String>,
    /// The channel ids it names that no `NotifyChannel` of the instance
    /// declares (dynamic channels, or a typo).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmanaged_channels: Vec<String>,
    /// The `metadata.generation` rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<i64>,
    /// The `metadata.generation` every pod of the instance confirmed
    /// loading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_generation: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RenderState {
    /// In the configuration.
    Rendered,
    /// An older object of the same kind holds its id.
    Conflict,
    /// A Secret it names is missing.
    SecretMissing,
    /// A Secret it names doesn't opt in.
    SecretNotAllowed,
    /// Its spec can't be rendered (Notify would refuse it).
    Invalid,
}

/// Ids of managed objects Notify still manages although their CR is gone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RefusedRemovals {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_accounts: Vec<String>,
}

impl RefusedRemovals {
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty() && self.service_accounts.is_empty()
    }
}
