//! The CRDs of the operator. Pure types: `serde`/`schemars` derives and
//! the kit's shared field types, no client code. Doc comments become the
//! CRD schema descriptions (`crdgen` → `deploy/crd/`).
//!
//! The kinds' Rust types carry their kind's name (`NotifyLocalInstance`,
//! the CRD's title); the workspace uses them through short re-exports
//! ([`LocalInstance`], [`Channel`]…).

pub mod channel;
pub mod common;
pub mod local_instance;
pub mod service_account;
pub mod status;
pub mod transform;
pub mod trust_policy;

/// API group shared by every CRD of this operator.
pub const GROUP: &str = "notify.weebo.io";

/// The version of every CRD of this operator.
pub const VERSION: &str = "v1alpha1";

/// The kit's common annotations under [`GROUP`]:
/// `notify.weebo.io/force-orphan` (release a CR without
/// cleanup), `allow-disruptive-update`, `import-pending`.
pub const ANNOTATIONS: weebo_kit_api::Annotations = weebo_kit_api::Annotations::new(GROUP);

/// The `app.kubernetes.io/managed-by` of every object the operator writes:
/// an existing object labelled with it (and the instance's name) is handed
/// over to the instance.
pub const MANAGED_BY: &str = "weebo-notify";

/// The label (`"true"`) a `Secret` must carry for the operator to read it
/// or the instance's pods to mount it.
pub const CREDENTIAL_LABEL: &str = "notify.weebo.io/credential";

/// The operator's own annotations.
pub mod annotations {
    /// On the pod template: the hash of what the server reads at boot only
    /// (what rolls the pods).
    pub const BOOT_HASH: &str = "notify.weebo.io/boot-hash";
    /// On an instance: the Notify tag allowed off the N → N+1 path.
    pub const APPROVED_UPGRADE: &str = "notify.weebo.io/approved-upgrade";
}

/// The kinds of the objects bound to an instance, as their CRDs name them
/// (and as [`status::object_key`] prefixes them).
pub mod kinds {
    pub const CHANNEL: &str = "NotifyChannel";
    pub const SERVICE_ACCOUNT: &str = "NotifyServiceAccount";
    pub const TRANSFORM: &str = "NotifyTransform";
    pub const TRUST_POLICY: &str = "NotifyTrustPolicy";
}

/// The Notify images the operator targets, and the versions it is tested
/// with: its configuration model (`notify-config`) is that of these
/// versions.
pub mod notify {
    /// The server's image repository.
    pub const SERVER_IMAGE: &str = "ghcr.io/batleforc/weebo-notify-v2";
    /// The web UI's image repository.
    pub const WEB_IMAGE: &str = "ghcr.io/batleforc/weebo-notify-v2-web";
    /// The version targeted: the default tag of both images, `v`-prefixed
    /// as Notify's release tags them (the git tag).
    pub const VERSION: &str = "v0.3.1";
    /// The tags an instance may run.
    pub const SUPPORTED_VERSIONS: &[&str] = &["v0.3.1"];
}

pub use channel::Channel;
pub use local_instance::LocalInstance;
pub use service_account::ServiceAccount;
pub use status::{InstanceStatus, ObjectStatus};
pub use transform::Transform;
pub use trust_policy::TrustPolicy;

#[cfg(test)]
mod tests {
    use kube::Resource;

    use super::*;

    #[test]
    fn the_kind_names_are_the_crds() {
        assert_eq!(Channel::kind(&()), kinds::CHANNEL);
        assert_eq!(ServiceAccount::kind(&()), kinds::SERVICE_ACCOUNT);
        assert_eq!(Transform::kind(&()), kinds::TRANSFORM);
        assert_eq!(TrustPolicy::kind(&()), kinds::TRUST_POLICY);
    }
}
