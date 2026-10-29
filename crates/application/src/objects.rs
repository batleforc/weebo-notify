//! The objects bound to an instance — channels, service accounts,
//! transforms, trust policies — as the instance's pass sees them, and who
//! keeps an id two of them claim.
//!
//! An id is unique per kind on an instance: of two objects claiming it, the
//! older (`creationTimestamp`, then `namespace/name`) is rendered, the
//! other is `IdConflict`. Trust policies have no id: they never conflict.

use std::collections::BTreeMap;

use api::channel::ChannelSpec;
use api::service_account::ServiceAccountSpec;
use api::transform::TransformSpec;
use api::trust_policy::TrustPolicySpec;
use k8s_openapi::jiff::Timestamp;

/// Which of the four kinds an object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKind {
    ServiceAccount,
    Channel,
    Transform,
    TrustPolicy,
}

impl ObjectKind {
    pub const ALL: [ObjectKind; 4] = [
        ObjectKind::ServiceAccount,
        ObjectKind::Channel,
        ObjectKind::Transform,
        ObjectKind::TrustPolicy,
    ];

    /// The CRD's kind (`NotifyChannel`…).
    pub const fn kind(self) -> &'static str {
        match self {
            ObjectKind::ServiceAccount => api::kinds::SERVICE_ACCOUNT,
            ObjectKind::Channel => api::kinds::CHANNEL,
            ObjectKind::Transform => api::kinds::TRANSFORM,
            ObjectKind::TrustPolicy => api::kinds::TRUST_POLICY,
        }
    }

    /// The kind a CRD's kind names.
    pub fn of(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.kind() == kind)
    }

    /// What the messages call it.
    pub const fn noun(self) -> &'static str {
        match self {
            ObjectKind::ServiceAccount => "service account",
            ObjectKind::Channel => "channel",
            ObjectKind::Transform => "transform",
            ObjectKind::TrustPolicy => "trust policy",
        }
    }

    /// Whether objects of this kind carry an id (all but trust policies).
    pub const fn has_id(self) -> bool {
        !matches!(self, ObjectKind::TrustPolicy)
    }
}

/// An object's spec, whichever its kind.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectSpec {
    ServiceAccount(ServiceAccountSpec),
    Channel(ChannelSpec),
    Transform(TransformSpec),
    TrustPolicy(TrustPolicySpec),
}

impl ObjectSpec {
    pub fn kind(&self) -> ObjectKind {
        match self {
            ObjectSpec::ServiceAccount(_) => ObjectKind::ServiceAccount,
            ObjectSpec::Channel(_) => ObjectKind::Channel,
            ObjectSpec::Transform(_) => ObjectKind::Transform,
            ObjectSpec::TrustPolicy(_) => ObjectKind::TrustPolicy,
        }
    }

    /// `spec.id`, when the kind has one and it is set.
    fn spec_id(&self) -> Option<&str> {
        match self {
            ObjectSpec::ServiceAccount(s) => s.id.as_deref(),
            ObjectSpec::Channel(s) => s.id.as_deref(),
            ObjectSpec::Transform(s) => s.id.as_deref(),
            ObjectSpec::TrustPolicy(_) => None,
        }
    }
}

/// An object as the instance's pass sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectView {
    pub namespace: String,
    pub name: String,
    pub created: Option<Timestamp>,
    pub generation: Option<i64>,
    pub spec: ObjectSpec,
    /// Being deleted (its finalizer holds it): out of the configuration.
    pub deleting: bool,
}

impl ObjectView {
    pub fn kind(&self) -> ObjectKind {
        self.spec.kind()
    }

    /// Its key in the instance's `status.objects`:
    /// `<kind>/<namespace>/<name>`.
    pub fn key(&self) -> String {
        api::status::object_key(self.kind().kind(), &self.namespace, &self.name)
    }

    /// `namespace/name`, what a conflict names.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }

    /// Its id in Notify: `spec.id`, or `metadata.name`; none for a trust
    /// policy.
    pub fn id(&self) -> Option<String> {
        self.kind()
            .has_id()
            .then(|| self.spec.spec_id().unwrap_or(&self.name).to_string())
    }
}

/// The id an object of `kind` named `name` has in Notify.
pub fn id_of(kind: ObjectKind, name: &str, spec_id: Option<&str>) -> Option<String> {
    kind.has_id().then(|| spec_id.unwrap_or(name).to_string())
}

/// The order objects are rendered in, and who wins an id: oldest first
/// (an object without a creation time last), then `namespace/name`.
pub fn sort(views: &mut [ObjectView]) {
    views.sort_by(|a, b| {
        (
            a.kind(),
            a.created.is_none(),
            a.created,
            &a.namespace,
            &a.name,
        )
            .cmp(&(
                b.kind(),
                b.created.is_none(),
                b.created,
                &b.namespace,
                &b.name,
            ))
    });
}

/// For each object that loses its id, the object (`namespace/name`) that
/// keeps it. `views` sorted by [`sort`].
pub fn conflicts(views: &[ObjectView]) -> BTreeMap<String, String> {
    let mut holders: BTreeMap<(ObjectKind, String), String> = BTreeMap::new();
    let mut lost = BTreeMap::new();
    for view in views {
        let Some(id) = view.id() else {
            continue;
        };
        match holders.get(&(view.kind(), id.clone())) {
            Some(winner) => {
                lost.insert(view.key(), winner.clone());
            }
            None => {
                holders.insert((view.kind(), id), view.reference());
            }
        }
    }
    lost
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(ns: &str, name: &str, created: Option<&str>, id: Option<&str>) -> ObjectView {
        ObjectView {
            namespace: ns.into(),
            name: name.into(),
            created: created.map(|c| c.parse().unwrap()),
            generation: Some(1),
            spec: ObjectSpec::ServiceAccount(ServiceAccountSpec {
                id: id.map(String::from),
                ..Default::default()
            }),
            deleting: false,
        }
    }

    #[test]
    fn the_oldest_object_keeps_an_id_then_the_first_by_name() {
        let mut views = vec![
            account("b", "ci", Some("2026-01-02T00:00:00Z"), None),
            account("a", "bot", Some("2026-01-01T00:00:00Z"), Some("ci")),
            account("c", "ci", Some("2026-01-01T00:00:00Z"), None),
            account("d", "other", None, Some("ci")),
            account("e", "alone", Some("2026-01-03T00:00:00Z"), None),
        ];
        sort(&mut views);
        let lost = conflicts(&views);
        assert_eq!(
            lost,
            BTreeMap::from([
                ("NotifyServiceAccount/b/ci".to_string(), "a/bot".to_string()),
                ("NotifyServiceAccount/c/ci".to_string(), "a/bot".to_string()),
                (
                    "NotifyServiceAccount/d/other".to_string(),
                    "a/bot".to_string()
                ),
            ])
        );
    }

    #[test]
    fn ids_are_per_kind_and_trust_policies_have_none() {
        let mut channel = account("a", "ci", Some("2026-01-02T00:00:00Z"), None);
        channel.spec = ObjectSpec::Channel(Default::default());
        let mut policy = account("a", "ci", Some("2026-01-02T00:00:00Z"), None);
        policy.spec = ObjectSpec::TrustPolicy(Default::default());
        let mut views = vec![
            account("a", "ci", Some("2026-01-01T00:00:00Z"), None),
            channel,
            policy.clone(),
            policy,
        ];
        sort(&mut views);
        assert!(conflicts(&views).is_empty());
        assert_eq!(views[3].id(), None);
        assert_eq!(ObjectKind::of("NotifyChannel"), Some(ObjectKind::Channel));
    }
}
