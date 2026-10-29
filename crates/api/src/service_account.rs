use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use weebo_kit_api::InstanceRef;

use crate::common::id_schema;
use crate::status::ObjectStatus;

/// The short names the rest of the workspace uses.
pub use self::NotifyServiceAccount as ServiceAccount;
pub use self::NotifyServiceAccountSpec as ServiceAccountSpec;

/// A managed service account of a Notify instance (`[[service_accounts]]`):
/// a machine identity channels grant scopes to. Rendered into the
/// instance's configuration; `Ready` once every pod of the instance
/// applied it. Deleting it **disables the account and revokes all its API
/// keys** in Notify; declaring it again re-enables it, but its keys stay
/// revoked.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[kube(
    group = "notify.weebo.io",
    version = "v1alpha1",
    kind = "NotifyServiceAccount",
    plural = "notifyserviceaccounts",
    shortname = "nsa",
    namespaced,
    status = "ObjectStatus",
    printcolumn = r#"{"name":"Instance","type":"string","jsonPath":".status.instance.name"}"#,
    printcolumn = r#"{"name":"Id","type":"string","jsonPath":".status.id"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].reason"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NotifyServiceAccountSpec {
    /// The instance it belongs to: unset, the only one of this namespace.
    /// An instance of another namespace must accept this one
    /// (`objectSelector.namespaces`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_ref: Option<InstanceRef>,
    /// Its id in Notify, unique on the instance: unset, `metadata.name`.
    /// **Immutable**: another id is another account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "id_schema")]
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "self == oldSelf", "message": "id is immutable: delete the service account and create another"}]))]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}
