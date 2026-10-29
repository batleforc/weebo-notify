use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use weebo_kit_api::InstanceRef;

use crate::common::Scope;
use crate::status::ObjectStatus;

/// The short names the rest of the workspace uses.
pub use self::NotifyTrustPolicy as TrustPolicy;
pub use self::NotifyTrustPolicySpec as TrustPolicySpec;

/// A federation trust policy of a Notify instance (`[[trust_policy]]`):
/// CI jobs whose OIDC token comes from `issuer` with subject `subject`
/// exchange it for a Notify token holding `scopes` on `channels`. Rendered
/// into the instance's configuration; `Ready` once every pod of the
/// instance applied it. A new `issuer` takes a restart of the instance's
/// pods (its verifier is built at boot): the operator rolls them.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[kube(
    group = "notify.weebo.io",
    version = "v1alpha1",
    kind = "NotifyTrustPolicy",
    plural = "notifytrustpolicies",
    shortname = "ntp",
    namespaced,
    status = "ObjectStatus",
    printcolumn = r#"{"name":"Instance","type":"string","jsonPath":".status.instance.name"}"#,
    printcolumn = r#"{"name":"Issuer","type":"string","jsonPath":".spec.issuer"}"#,
    printcolumn = r#"{"name":"Subject","type":"string","jsonPath":".spec.subject"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].reason"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NotifyTrustPolicySpec {
    /// The instance it belongs to: unset, the only one of this namespace.
    /// An instance of another namespace must accept this one
    /// (`objectSelector.namespaces`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_ref: Option<InstanceRef>,
    /// The CI forge's OIDC issuer, e.g.
    /// `https://token.actions.githubusercontent.com`.
    #[schemars(length(min = 1))]
    pub issuer: String,
    /// The token's `sub` claim, as the forge writes it.
    #[schemars(length(min = 1))]
    pub subject: String,
    /// The channel ids the exchanged token reaches.
    #[schemars(length(min = 1))]
    pub channels: Vec<String>,
    #[schemars(length(min = 1))]
    pub scopes: Vec<Scope>,
}
