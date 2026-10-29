use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use weebo_kit_api::{InstanceRef, LocalSecretKeyRef};

use crate::common::{Lang, Scope, id_schema};
use crate::status::ObjectStatus;

/// The short names the rest of the workspace uses.
pub use self::NotifyChannel as Channel;
pub use self::NotifyChannelSpec as ChannelSpec;

/// A managed channel of a Notify instance (`[[channels.managed]]`): its
/// policy, who may publish, subscribe and manage, where it forwards, what
/// it polls. Rendered into the instance's configuration; `Ready` once
/// every pod of the instance applied it. Deleting it archives the channel
/// in Notify (it becomes dynamic, its history kept) or deletes it with its
/// notifications, as the instance's `channels.onRemoved` says.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[kube(
    group = "notify.weebo.io",
    version = "v1alpha1",
    kind = "NotifyChannel",
    plural = "notifychannels",
    shortname = "nch",
    namespaced,
    status = "ObjectStatus",
    printcolumn = r#"{"name":"Instance","type":"string","jsonPath":".status.instance.name"}"#,
    printcolumn = r#"{"name":"Id","type":"string","jsonPath":".status.id"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].reason"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NotifyChannelSpec {
    /// The instance it belongs to: unset, the only one of this namespace.
    /// An instance of another namespace must accept this one
    /// (`objectSelector.namespaces`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_ref: Option<InstanceRef>,
    /// Its id in Notify, unique on the instance, what publishers address:
    /// unset, `metadata.name`. **Immutable**: another id is another
    /// channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "id_schema")]
    #[schemars(extend("x-kubernetes-validations" = [{"rule": "self == oldSelf", "message": "id is immutable: delete the channel and create another"}]))]
    pub id: Option<String>,
    /// The name the inbox shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How long a notification is kept when its publisher says nothing
    /// (`48h`, `30d`; `"0"`, for ever, needs the instance's
    /// `retention.allowForever`). Unset: the instance's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ttl: Option<String>,
    /// The longest TTL a publisher may ask for; a longer one is clamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ttl: Option<String>,
    /// Inactivity after which Notify closes an open notification
    /// (revisions and acknowledgements count as activity); `"0"`:
    /// publishers close their own. Unset: one week.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_priority: Option<Priority>,
    /// Record who read each notification.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receipts: bool,
    /// The most notifications one principal may publish per minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub publish_per_min: Option<u32>,
    /// Who holds which scopes. A `serviceAccount` names a service account
    /// id of the same instance (a `NotifyServiceAccount`). At most 256.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub access: Vec<Access>,
    /// CI jobs that may publish here with their forge's OIDC token: a
    /// trust policy scoped to this channel, taking priority over the
    /// instance's `NotifyTrustPolicy`s.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub publishers: Vec<Publisher>,
    /// Where every notification is also delivered. At most 16.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub sinks: Vec<Sink>,
    /// RSS, Atom and JSON feeds polled into the channel. At most 32.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    #[schemars(extend("x-kubernetes-validations" = [
        {"rule": "self.all(f, self.exists_one(o, o.url == f.url))", "message": "a feed url appears once per channel"}
    ]))]
    pub feeds: Vec<Feed>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Min,
    Low,
    Default,
    High,
    Urgent,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Min => "min",
            Priority::Low => "low",
            Priority::Default => "default",
            Priority::High => "high",
            Priority::Urgent => "urgent",
        }
    }
}

/// One subject and the scopes it holds.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "[has(self.oidcGroup), has(self.oidcSub), has(self.email), has(self.serviceAccount)].filter(x, x).size() == 1", "message": "an access binding names exactly one of oidcGroup, oidcSub, email and serviceAccount"}
]))]
pub struct Access {
    /// Everyone in this group of the identity provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_group: Option<String>,
    /// One person, by their `sub` at the identity provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_sub: Option<String>,
    /// One person, by their verified email; takes effect at their first
    /// login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// A service account id of the instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account: Option<String>,
    #[schemars(length(min = 1))]
    pub scopes: Vec<Scope>,
}

/// A CI identity allowed to publish here.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Publisher {
    /// The forge's OIDC issuer.
    #[schemars(length(min = 1))]
    pub issuer: String,
    /// The token's `sub` claim.
    #[schemars(length(min = 1))]
    pub subject: String,
    #[schemars(length(min = 1))]
    pub scopes: Vec<Scope>,
}

/// An outbound delivery of every notification of the channel.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "self.type != 'webhook' || (has(self.url) && has(self.secretRef) && !has(self.address))", "message": "a webhook sink has a url and a secretRef (it signs every delivery), no address"},
    {"rule": "self.type != 'email' || (has(self.address) && !has(self.url) && !has(self.secretRef))", "message": "an email sink has an address only; the relay is the instance's smtp"}
]))]
pub struct Sink {
    #[serde(rename = "type")]
    pub r#type: SinkType,
    /// `webhook`: where it posts, http(s), with no credentials in it (the
    /// url is stored and logged): put them in `secretRef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `email`: where it goes, through the instance's `smtp` relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// `webhook`: the secret each delivery is signed with, in a `Secret` of
    /// the channel's namespace labelled `notify.weebo.io/credential:
    /// "true"`. Mounted into the instance's pods, never written into its
    /// configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<LocalSecretKeyRef>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SinkType {
    Webhook,
    Email,
}

impl SinkType {
    pub fn as_str(self) -> &'static str {
        match self {
            SinkType::Webhook => "webhook",
            SinkType::Email => "email",
        }
    }
}

/// A feed Notify's leader polls, publishing each new item as `publisher`.
/// The first poll only records what is already there.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Feed {
    /// http(s), no `user:pass@`. Changing it makes another feed, which
    /// primes again.
    #[schemars(length(min = 1, max = 2048))]
    pub url: String,
    /// The service account it publishes as: it must hold `publish` in this
    /// channel's `access`.
    pub publisher: String,
    /// How often it is polled, from `5m` to `24h`. Notify: `30m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// Added to every item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The most new items one poll publishes, from 1 to 50. Notify: 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 50))]
    pub max_per_poll: Option<u32>,
    /// Poll a non-public address (refused otherwise).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_private_network: bool,
    /// The language of the "Open" label and the summary title.
    #[serde(default, skip_serializing_if = "Lang::is_default")]
    pub lang: Lang,
}
