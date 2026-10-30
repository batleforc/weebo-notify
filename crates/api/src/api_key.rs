use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use weebo_kit_api::{InstanceRef, ObjectRef, SecretTarget};

use crate::common::Scope;
use crate::status::{BoundInstance, Condition};

/// The short names the rest of the workspace uses.
pub use self::NotifyApiKey as ApiKey;
pub use self::NotifyApiKeySpec as ApiKeySpec;

/// An API key of a `NotifyServiceAccount`, minted by the operator through
/// the instance's key issuer and written to `secretTargets` as
/// `NOTIFY_API_KEY` (`wnk_…`) and `NOTIFY_URL`.
///
/// A key can't be read back: a lost target, a new
/// `notify.weebo.io/rotate-secret` value, or a change of the service
/// account, scopes or channels mints a new key. The new key is written to
/// every target before the previous one is revoked. Deleting the CR
/// revokes its key and deletes its targets; deleting the service account
/// revokes it too.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "notify.weebo.io",
    version = "v1alpha1",
    kind = "NotifyApiKey",
    plural = "notifyapikeys",
    shortname = "nak",
    namespaced,
    status = "ApiKeyStatus",
    printcolumn = r#"{"name":"Instance","type":"string","jsonPath":".status.instance.name"}"#,
    printcolumn = r#"{"name":"Account","type":"string","jsonPath":".status.serviceAccount"}"#,
    printcolumn = r#"{"name":"Prefix","type":"string","jsonPath":".status.keyPrefix"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].reason"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NotifyApiKeySpec {
    /// The instance: unset, the only one of this namespace. An instance of
    /// another namespace must accept this one
    /// (`objectSelector.namespaces`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_ref: Option<InstanceRef>,
    /// The `NotifyServiceAccount` of this namespace the key acts as, bound
    /// to the same instance.
    #[schemars(extend("x-weebo-link" = { "to": [{ "group": "notify.weebo.io", "kind": "NotifyServiceAccount" }] }))]
    pub service_account_ref: ObjectRef,
    /// What the key may do. Notify refuses a scope the service account
    /// doesn't hold in every channel listed.
    #[schemars(length(min = 1))]
    pub scopes: Vec<Scope>,
    /// The channel ids the key may act in.
    #[schemars(length(min = 1))]
    pub channels: Vec<String>,
    /// Defaults to one target named after the CR, in its namespace, on the
    /// operator's default backend (`secretStore.defaultBackend` of its
    /// chart). A Vault target uses the operator's Vault connection and
    /// stays under `<pathPrefix>/<namespace>/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    // The keys each target holds, for the docs' CRD IDE (as `reconcile_api_key` writes them).
    #[schemars(extend("x-weebo-keys" = ["NOTIFY_API_KEY", "NOTIFY_URL"]))]
    pub secret_targets: Vec<SecretTarget>,
}

/// The status of an API key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyStatus {
    /// The `metadata.generation` the last successful reconcile applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// The instance this key was resolved to and stays bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<BoundInstance>,
    /// The service account id the key acts as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account: Option<String>,
    /// The key's id in Notify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// The key's public prefix (`wnk_<prefix>_…`), as Notify lists it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    /// The `notify.weebo.io/rotate-secret` value the key was minted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rotation: Option<String>,
    /// `Ready`, carrying one of the operator's reason codes.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// The keys of each target.
pub mod keys {
    pub const API_KEY: &str = "NOTIFY_API_KEY";
    pub const URL: &str = "NOTIFY_URL";
}

/// The label of the keys the operator mints for `namespace/name`: what
/// tells them apart from keys minted another way.
pub fn label(namespace: &str, name: &str) -> String {
    format!("{}:{namespace}/{name}", crate::MANAGED_BY)
}
