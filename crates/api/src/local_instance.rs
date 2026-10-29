use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Affinity, LocalObjectReference, ResourceRequirements, Toleration,
};
use k8s_openapi::api::networking::v1::NetworkPolicyPeer;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use weebo_kit_api::{AttachNamespaces, LocalSecretKeyRef, NamespacesFrom, SharedWith};

use crate::status::InstanceStatus;

/// The short names the rest of the workspace uses.
pub use self::NotifyLocalInstance as LocalInstance;
pub use self::NotifyLocalInstanceSpec as LocalInstanceSpec;

/// A Notify instance the operator runs in this namespace: the server's
/// `Deployment` and its migration `Job`, the web UI, their configuration
/// (rendered from this spec and the channels, service accounts,
/// transforms and trust policies that attach to it) and the objects
/// around them. The database is yours: the instance only reads its URL.
/// `Ready` tells whether every pod runs the desired configuration.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "notify.weebo.io",
    version = "v1alpha1",
    kind = "NotifyLocalInstance",
    plural = "notifylocalinstances",
    shortname = "nli",
    namespaced,
    status = "InstanceStatus",
    printcolumn = r#"{"name":"Endpoint","type":"string","jsonPath":".status.endpoint"}"#,
    printcolumn = r#"{"name":"Version","type":"string","jsonPath":".status.version"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].reason"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    // The instance's name names its Services (`<name>`, `<name>-web`), DNS-1035
    // labels, and the `app.kubernetes.io/instance` label of what it owns.
    validation = Rule::new(
        "size(self.metadata.name) <= 50 && self.metadata.name.matches('^[a-z]([-a-z0-9]*[a-z0-9])?$')"
    ).message("the instance's name names its Services (and <name>-web): a DNS-1035 label (lowercase letters, digits and '-', starting with a letter), at most 50 characters")
)]
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "self.replicas <= 1 || !has(self.attachments.filesystem)", "message": "more than one replica needs S3 attachments: a filesystem store lives on one pod's volume"}
]))]
#[serde(rename_all = "camelCase")]
pub struct NotifyLocalInstanceSpec {
    /// The server's image. Its repository must be one the operator allows
    /// (`managedInstances.allowedImages`). Its tag must be a Notify version
    /// the operator supports, never below the version the database was last
    /// migrated to (`status.migratedVersion`), and at most one minor ahead of
    /// it — unless the annotation `notify.weebo.io/approved-upgrade` names
    /// that tag. A new tag is migrated first (a `<name>-migrate-<hash>`
    /// `Job`, condition `Migrated`); the pods keep the previous one until it
    /// succeeds.
    #[serde(default, skip_serializing_if = "Image::is_default")]
    pub image: Image,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<LocalObjectReference>,
    /// The server's pods. The API is stateless; background jobs run on one
    /// of them, elected through the database.
    #[serde(default = "two")]
    #[schemars(range(min = 0))]
    pub replicas: i32,
    /// Unset: requests 100m CPU / 128Mi, limit 512Mi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "open_object")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pod_annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "open_list")]
    pub tolerations: Vec<Toleration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "open_object")]
    pub affinity: Option<Affinity>,
    /// The `NetworkPolicy` the operator always puts around the pods: they
    /// answer the operator, the ingress controller and these peers; they
    /// reach DNS, the database, the object store, the identity provider and
    /// these.
    #[serde(default, skip_serializing_if = "NetworkPolicy::is_default")]
    pub network_policy: NetworkPolicy,
    pub database: Database,
    pub attachments: Attachments,
    pub oidc: Oidc,
    /// The platform role, declared here only: no API path grants it.
    #[serde(default, skip_serializing_if = "Admin::is_default")]
    pub admin: Admin,
    #[serde(default, skip_serializing_if = "Server::is_default")]
    pub server: Server,
    /// The relay `email` sinks hand their mail to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp: Option<Smtp>,
    /// Web Push (RFC 8292). Unset: no push transport, every push degrades
    /// to a poke the client fetches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_push: Option<WebPush>,
    /// Where traces go. Unset: none exported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<Telemetry>,
    #[serde(default, skip_serializing_if = "Retention::is_default")]
    pub retention: Retention,
    #[serde(default, skip_serializing_if = "Sinks::is_default")]
    pub sinks: Sinks,
    /// The key the CI tokens the instance mints are signed with. Rendered
    /// as soon as a trust policy or a channel's `publishers` is bound (or a
    /// key is given here): without it Notify ignores both.
    #[serde(default, skip_serializing_if = "Federation::is_default")]
    pub federation: Federation,
    #[serde(default)]
    pub channels: Channels,
    #[serde(default)]
    pub reconcile: Reconcile,
    /// The web UI: a `<name>-web` Deployment serving the browser client,
    /// pointed at this instance's API and identity provider.
    #[serde(default)]
    pub web: Web,
    /// Unset: no `Ingress`; reach the `Service`s some other way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<Ingress>,
    /// A `ServiceMonitor` on `/metrics`, created when its CRD exists.
    #[serde(default)]
    pub service_monitor: ServiceMonitor,
    /// Which `NotifyChannel`s, `NotifyServiceAccount`s, `NotifyTransform`s
    /// and `NotifyTrustPolicy`s the instance takes, and from which
    /// namespaces.
    #[serde(default, skip_serializing_if = "ObjectSelector::is_default")]
    pub object_selector: ObjectSelector,
    /// Keys of Notify's `config.toml` no typed field renders, merged last.
    /// May not hold `channels.managed`, `service_accounts`, `transforms` or
    /// `trust_policy` (their CRs are the only source), nor a secret: put
    /// those in a `Secret` and reference it with an `env:`/`file:` value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(schema_with = "open_object")]
    pub extra_config: BTreeMap<String, serde_json::Value>,
}

fn open_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true,
    })
}

fn open_list(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "items": {"type": "object", "x-kubernetes-preserve-unknown-fields": true},
    })
}

fn one() -> i32 {
    1
}

fn two() -> i32 {
    2
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    /// Default: `ghcr.io/batleforc/weebo-notify` (the server),
    /// `ghcr.io/batleforc/weebo-notify-web` (the web UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Default: the Notify version the operator targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_policy: Option<PullPolicy>,
}

impl Image {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
pub enum PullPolicy {
    Always,
    #[default]
    IfNotPresent,
    Never,
}

impl PullPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            PullPolicy::Always => "Always",
            PullPolicy::IfNotPresent => "IfNotPresent",
            PullPolicy::Never => "Never",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicy {
    /// More peers that may reach the API and the web UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "open_list")]
    pub ingress_from: Vec<NetworkPolicyPeer>,
    /// Where the pods may connect besides DNS and 443/5432 anywhere: an SMTP
    /// relay, a collector, the hosts of webhooks and feeds on other ports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "open_list")]
    pub egress_to: Vec<NetworkPolicyPeer>,
}

impl NetworkPolicy {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The Postgres database, yours to run (CloudNativePG, a managed service…).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Database {
    /// Its URL (`postgres://user:password@host:5432/notify`), in a
    /// `Secret` of this namespace labelled `notify.weebo.io/credential:
    /// "true"`. Read at boot: changing it rolls the pods.
    pub url_secret_ref: LocalSecretKeyRef,
    /// The pool size, from 10. Notify also holds one connection per
    /// background job it leads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 10))]
    pub max_connections: Option<u32>,
}

/// Where attachments are stored: exactly one of `s3` and `filesystem`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {"rule": "has(self.s3) != has(self.filesystem)", "message": "attachments are stored in exactly one of s3 and filesystem"}
]))]
pub struct Attachments {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3>,
    /// A volume the operator creates (`<name>-attachments`), served through
    /// the server's own routes: one replica only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<Filesystem>,
}

/// An S3-compatible bucket (RustFS, MinIO, AWS…).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct S3 {
    #[schemars(length(min = 1))]
    pub endpoint: String,
    #[schemars(length(min = 1))]
    pub bucket: String,
    /// Notify: `us-east-1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// A `Secret` of this namespace labelled `notify.weebo.io/credential:
    /// "true"`.
    pub credentials_secret_ref: S3Credentials,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct S3Credentials {
    pub name: String,
    #[serde(default = "access_key_id")]
    pub access_key_id_key: String,
    #[serde(default = "secret_access_key")]
    pub secret_access_key_key: String,
}

fn access_key_id() -> String {
    "AWS_ACCESS_KEY_ID".into()
}

fn secret_access_key() -> String {
    "AWS_SECRET_ACCESS_KEY".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Filesystem {
    /// The claim's size. **Can only grow.**
    #[serde(default = "attachments_size")]
    pub size: Quantity,
    /// Unset: the cluster's default class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// The API's external base URL, which attachment links are signed
    /// against (`https://notify.example.com`).
    #[schemars(length(min = 1))]
    pub public_base_url: String,
    /// The key the links are signed with (at least 32 bytes), in a
    /// labelled `Secret`. Unset: the operator generates one
    /// (`<name>-attachments-key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret_ref: Option<LocalSecretKeyRef>,
}

fn attachments_size() -> Quantity {
    Quantity("5Gi".into())
}

/// The identity provider people sign in with. Read at boot: changing it
/// rolls the pods.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Oidc {
    #[schemars(length(min = 1))]
    pub issuer: String,
    /// The public client the web UI and the CLI sign in with.
    #[schemars(length(min = 1))]
    pub client_id: String,
    /// More audiences a token may carry; the client id always is one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audiences: Vec<String>,
    /// The claim carrying group membership. Notify: `groups`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups_claim: Option<String>,
    /// Trust an `email` the issuer does not mark verified. Only for an
    /// issuer that never sends `email_verified`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trust_unverified_email: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Admin {
    /// The identity provider's groups whose members administer the
    /// instance. Read at boot: changing it rolls the pods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oidc_groups: Vec<String>,
}

impl Admin {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    /// Origins allowed to call the API from a browser, besides the web
    /// UI's own (added by the operator).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cors_origins: Vec<String>,
    /// Serve the API reference at `/api/docs`. Notify: `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<bool>,
}

impl Server {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Smtp {
    /// `smtp://host:port` inside a cluster, `smtps://…` across one.
    #[schemars(length(min = 1))]
    pub url: String,
    /// The envelope sender.
    #[schemars(length(min = 1))]
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// In a labelled `Secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_secret_ref: Option<LocalSecretKeyRef>,
    /// Where the instance is reachable, for the mail's link; unset, no
    /// link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permalink_base: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebPush {
    /// The P-256 VAPID private key (PEM), in a labelled `Secret`. Rotating
    /// it silently invalidates every existing subscription.
    pub vapid_key_secret_ref: LocalSecretKeyRef,
    /// `mailto:…` or `https://…`: how a push relay reaches you.
    #[schemars(length(min = 1))]
    pub contact: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Telemetry {
    /// The collector's OTLP HTTP base URL, e.g. `http://collector:4318`.
    #[schemars(length(min = 1))]
    pub otlp_endpoint: String,
    /// Unset: the instance's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    /// The fraction of root traces recorded, `"0.0"` to `"1.0"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_ratio: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    /// The TTL of a notification on a channel that sets none (`30d`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ttl: Option<String>,
    /// The longest TTL any channel may set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ttl: Option<String>,
    /// Let a channel keep notifications for ever (`defaultTtl: "0"`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_forever: bool,
    /// How long audit entries are kept, at least `90d`. Unset: for ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<String>,
}

impl Retention {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Sinks {
    /// The key webhook and feed urls are fingerprinted under (at least 32
    /// bytes), in a labelled `Secret`. Unset: the operator generates one
    /// (`<name>-fingerprint-key`). Read at boot; rotating it has a runbook
    /// (Notify's docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_key_secret_ref: Option<LocalSecretKeyRef>,
    /// How long a dead letter is kept (`30d`, Notify's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter_retention: Option<String>,
    /// The keys used before `fingerprintKeySecretRef`, for one rollout after
    /// a rotation (Notify re-keys the stored ids at boot), in labelled
    /// `Secret`s. Without `"` or `\` (base64 or hex keys are fine).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous_fingerprint_key_secret_refs: Vec<LocalSecretKeyRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Federation {
    /// The Ed25519 private key (PKCS#8 PEM, `openssl genpkey -algorithm
    /// ed25519`), in a labelled `Secret`. Unset: the operator generates one
    /// (`<name>-federation-key`). Read at boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key_secret_ref: Option<LocalSecretKeyRef>,
    /// The minted tokens' audience. Notify: `weebo-notify`; change it when
    /// instances share an ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
}

impl Federation {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Sinks {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Channels {
    /// What a deleted `NotifyChannel` does to its channel: `Archive` keeps
    /// it and its history (it becomes dynamic, managed from the UI);
    /// `Delete` deletes it, history included.
    #[serde(default)]
    pub on_removed: OnRemoved,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
pub enum OnRemoved {
    #[default]
    Archive,
    Delete,
}

impl OnRemoved {
    pub fn as_str(self) -> &'static str {
        match self {
            OnRemoved::Archive => "archive",
            OnRemoved::Delete => "delete",
        }
    }
}

/// Notify's guard on removals: a pass that would remove more channels and
/// service accounts than this applies everything else and refuses the
/// removals (their CRs keep their finalizer, `RemovalRefused`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Reconcile {
    #[serde(default = "five")]
    pub max_removals: u32,
    /// Lift the guard, for the one change that removes a lot on purpose;
    /// unset it after.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_mass_removal: bool,
}

fn five() -> u32 {
    5
}

impl Default for Reconcile {
    fn default() -> Self {
        Self {
            max_removals: five(),
            allow_mass_removal: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Web {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Its repository must be one the operator allows.
    #[serde(default, skip_serializing_if = "Image::is_default")]
    pub image: Image,
    #[serde(default = "one")]
    #[schemars(range(min = 0))]
    pub replicas: i32,
    /// Unset: requests 10m CPU / 16Mi, limit 64Mi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "open_object")]
    pub resources: Option<ResourceRequirements>,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            enabled: true,
            image: Image::default(),
            replicas: one(),
            resources: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Ingress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    /// The API's host. `/healthz`, `/readyz` and `/metrics` are not routed.
    pub api: IngressHost,
    /// The web UI's host (another one: the UI and the API are separate
    /// origins). Unset with the web UI on: no `Ingress` for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<IngressHost>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IngressHost {
    #[schemars(length(min = 1))]
    pub host: String,
    /// The `Secret` holding its certificate (cert-manager's, often).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceMonitor {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl Default for ServiceMonitor {
    fn default() -> Self {
        Self {
            enabled: true,
            labels: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectSelector {
    /// Only the objects whose labels match; all of them when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector>,
    /// The namespaces whose objects may attach to this instance (with an
    /// `instanceRef` naming it): its own always.
    #[serde(default, skip_serializing_if = "AttachNamespaces::is_default")]
    pub namespaces: AttachNamespaces,
}

impl ObjectSelector {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// What the instance shares with the objects of other namespaces (its
/// `objectSelector.namespaces`).
pub fn object_sharing(instance: &LocalInstance) -> Option<SharedWith> {
    instance.spec.object_selector.namespaces.shared_with()
}

/// The names of what an instance owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Names<'a>(pub &'a str);

impl Names<'_> {
    /// The server's `Deployment`, `Service`, `ServiceAccount`,
    /// `NetworkPolicy`, `PodDisruptionBudget` and `ServiceMonitor`.
    pub fn server(&self) -> String {
        self.0.to_string()
    }
    /// The web UI's `Deployment`, `Service` and `Ingress`.
    pub fn web(&self) -> String {
        format!("{}-web", self.0)
    }
    /// The `ConfigMap` holding `config.toml`.
    pub fn config(&self) -> String {
        format!("{}-config", self.0)
    }
    /// The migration `Job` of a version (`<name>-migrate-<hash8>`).
    pub fn migrate_prefix(&self) -> String {
        format!("{}-migrate", self.0)
    }
    /// The generated fingerprint key.
    pub fn fingerprint_key(&self) -> String {
        format!("{}-fingerprint-key", self.0)
    }
    /// The filesystem store's claim.
    pub fn attachments(&self) -> String {
        format!("{}-attachments", self.0)
    }
    /// The generated filesystem link key.
    pub fn attachments_key(&self) -> String {
        format!("{}-attachments-key", self.0)
    }
    /// The generated federation signing key.
    pub fn federation_key(&self) -> String {
        format!("{}-federation-key", self.0)
    }
    /// The sinks' secrets, copied next to the pods.
    pub fn sinks(&self) -> String {
        format!("{}-sinks", self.0)
    }
}

#[cfg(test)]
mod tests {
    use kube::CustomResourceExt;

    use super::*;

    fn minimal() -> serde_json::Value {
        serde_json::json!({
            "database": {"urlSecretRef": {"name": "db", "key": "url"}},
            "attachments": {"s3": {"endpoint": "http://rustfs:9000", "bucket": "notify", "credentialsSecretRef": {"name": "s3"}}},
            "oidc": {"issuer": "https://id.example", "clientId": "notify"},
        })
    }

    #[test]
    fn the_name_the_short_name_and_the_store_are_checked() {
        let crd = serde_json::to_value(LocalInstance::crd()).unwrap();
        assert_eq!(crd["spec"]["names"]["shortNames"][0], "nli");
        let schema = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"];
        let rules = &schema["x-kubernetes-validations"];
        assert!(rules.to_string().contains("DNS-1035"), "{rules}");
        let rules = &schema["properties"]["spec"]["x-kubernetes-validations"];
        assert!(rules.to_string().contains("S3 attachments"), "{rules}");
    }

    #[test]
    fn a_minimal_instance_reads_with_its_defaults() {
        let spec: LocalInstanceSpec = serde_json::from_value(minimal()).unwrap();
        assert_eq!(spec.replicas, 2);
        assert!(spec.web.enabled);
        assert_eq!(spec.web.replicas, 1);
        assert!(spec.service_monitor.enabled);
        assert_eq!(spec.channels.on_removed, OnRemoved::Archive);
        assert_eq!(spec.reconcile.max_removals, 5);
        let s3 = spec.attachments.s3.unwrap();
        assert_eq!(
            s3.credentials_secret_ref.access_key_id_key,
            "AWS_ACCESS_KEY_ID"
        );
    }

    #[test]
    fn an_instance_shares_only_through_its_selector() {
        let spec: LocalInstanceSpec = serde_json::from_value(minimal()).unwrap();
        let mut instance = LocalInstance::new("notify", spec);
        assert_eq!(object_sharing(&instance), None);
        instance.spec.object_selector.namespaces.from = NamespacesFrom::All;
        assert!(object_sharing(&instance).is_some());
    }
}
