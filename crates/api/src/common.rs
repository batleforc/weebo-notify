//! Field types the object kinds share.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a principal may do in a channel.
#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Write notifications and revisions.
    Publish,
    /// Read the inbox, the stream and the sync backlog.
    Subscribe,
    /// Administer the channel (members, keys, policy); not the instance.
    Manage,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Publish => "publish",
            Scope::Subscribe => "subscribe",
            Scope::Manage => "manage",
        }
    }
}

/// The language of the strings Notify writes for a feed or a transform.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Fr,
}

impl Lang {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Fr => "fr",
        }
    }
}

/// An id as Notify stores it (its `Slug`): lowercase letters, digits, `-`,
/// `_` and `.`, starting and ending with a letter or a digit, at most 64
/// characters.
pub(crate) fn id_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": 64,
        "pattern": "^[a-z0-9]([-_.a-z0-9]*[a-z0-9])?$",
    })
}

/// A transform's id, which Notify also uses as a path segment: lowercase
/// letters, digits and `-`, at most 64 characters.
pub(crate) fn transform_id_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": 64,
        "pattern": "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$",
    })
}
