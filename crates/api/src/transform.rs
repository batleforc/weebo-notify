use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use weebo_kit_api::InstanceRef;

use crate::common::{Lang, transform_id_schema};
use crate::status::ObjectStatus;

/// The short names the rest of the workspace uses.
pub use self::NotifyTransform as Transform;
pub use self::NotifyTransformSpec as TransformSpec;

/// A JSON webhook transform of a Notify instance (`[[transforms]]`): CEL
/// programs that turn the JSON a third party posts to a channel into
/// notifications. The expressions are checked with Notify's own compiler
/// when the CR is applied. Rendered into the instance's configuration;
/// `Ready` once every pod of the instance applied it. Holds no state in
/// Notify: deleting it removes it at once.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[kube(
    group = "notify.weebo.io",
    version = "v1alpha1",
    kind = "NotifyTransform",
    plural = "notifytransforms",
    shortname = "ntr",
    namespaced,
    status = "ObjectStatus",
    printcolumn = r#"{"name":"Instance","type":"string","jsonPath":".status.instance.name"}"#,
    printcolumn = r#"{"name":"Id","type":"string","jsonPath":".status.id"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].reason"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NotifyTransformSpec {
    /// The instance it belongs to: unset, the only one of this namespace.
    /// An instance of another namespace must accept this one
    /// (`objectSelector.namespaces`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_ref: Option<InstanceRef>,
    /// Its id in Notify, unique on the instance and the path segment
    /// producers post to (`/api/v1/compat/json/{id}/{channel}`): unset,
    /// `metadata.name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "transform_id_schema")]
    pub id: Option<String>,
    /// The channel ids it publishes into. One that no `NotifyChannel`
    /// declares is accepted (it may be a dynamic channel), with an advisory.
    #[schemars(length(min = 1))]
    pub channels: Vec<String>,
    /// The language of the strings it writes: the "Open" label, the
    /// truncation marker.
    #[serde(default, skip_serializing_if = "Lang::is_default")]
    pub lang: Lang,
    /// The expressions below are CEL over `body`, the received JSON (and
    /// `item` with `split`); a literal string needs CEL quotes
    /// (`'"news"'`). CEL, optional: a list, one notification per element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    /// With `split` only: the most elements one post may carry (more is a
    /// `422`). Notify: 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 50))]
    pub max_items: Option<u32>,
    /// CEL: a string.
    #[schemars(length(min = 1))]
    pub title: String,
    /// A plain string, the title when `title` fails or is empty; unset, the
    /// id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_fallback: Option<String>,
    /// CEL: a string, Markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// CEL: an http(s) URL, the "Open" button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// CEL: a string or a number; unset, a digest of the element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// CEL: a string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
    /// CEL: a list of strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<String>,
    /// CEL: `min`, `low`, `default`, `high` or `urgent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// CEL: `success`, `failure`, `warning`, `info` or `cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// CEL: `open`, `working` or `closed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// CEL: a button label, or `null` for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<String>,
    /// `false`: an element already published is skipped; `true`: it is
    /// revised.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update: bool,
    /// The received JSON appended to the body: `item` (with `split`) or the
    /// whole `payload`.
    #[serde(default, skip_serializing_if = "RawBody::is_default")]
    pub raw: RawBody,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RawBody {
    /// Nothing.
    #[default]
    Off,
    /// The item `split` selected.
    Item,
    /// The whole post.
    Payload,
}

impl RawBody {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RawBody::Off => "off",
            RawBody::Item => "item",
            RawBody::Payload => "payload",
        }
    }
}
