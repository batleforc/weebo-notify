//! The objects an instance owns, built from its spec and its rendered
//! configuration ([`crate::render::Rendered`]). Pure: the pass decides
//! which image the server runs (migration first), which generated keys are
//! new and what the sinks' secrets hold; this module only shapes the
//! objects.
//!
//! - `<name>-config` (the file), rewritten in place: Notify reloads it.
//!   Only the boot hash is on the pod template — a hot change never rolls.
//! - The generated keys (create-only), `<name>-sinks` (the sinks' secrets,
//!   copied, rewritten in place: mounted whole, a new one needs no
//!   restart).
//! - The server's `Deployment`, `Service`, `ServiceAccount`,
//!   `PodDisruptionBudget` (more than one replica), `NetworkPolicy`, the
//!   filesystem store's claim, the migration `Job` of a new image.
//! - The web UI's `Deployment`, `Service` and `NetworkPolicy`, the
//!   `Ingress`es, the `ServiceMonitor`.

use std::collections::BTreeMap;

use api::local_instance::{LocalInstanceSpec, Names};
use k8s_openapi::ByteString;
use k8s_openapi::api::apps::v1::{
    Deployment, DeploymentSpec, DeploymentStrategy, RollingUpdateDeployment,
};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, KeyToPath, Lifecycle, LifecycleHandler, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, Probe,
    ResourceRequirements, Secret, SecretKeySelector, SecretVolumeSource, Service, ServiceAccount,
    ServicePort, ServiceSpec, SleepAction, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, IngressTLS, NetworkPolicyEgressRule, NetworkPolicyPort,
    ServiceBackendPort,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use weebo_kit_application::workload::{
    OperatorPod, PolicyRules, hashed_name, network_policy, restricted_container_security,
    restricted_pod_security,
};

use crate::ports::OwnedObject;
use crate::render::{Rendered, paths};

/// The server image's user (distroless `nonroot`).
pub const NOTIFY_UID: i64 = 65532;
/// The web image's user (`nginx-unprivileged`).
pub const WEB_UID: i64 = 101;
/// The web UI's port (the image's nginx).
pub const WEB_PORT: i32 = 8080;

/// The template annotation of the server's image: read back from the
/// running `Deployment` (which tag it runs while a migration waits).
pub const IMAGE_ANNOTATION: &str = "notify.weebo.io/image";
/// On the server's and the migration's pods: what the server's
/// `NetworkPolicy` selects (both reach the database).
pub const SERVER_POD_LABEL: &str = "notify.weebo.io/server";

/// What the objects are built from.
#[derive(Debug, Clone)]
pub struct WorkloadInput<'a> {
    pub name: &'a str,
    pub namespace: &'a str,
    pub spec: &'a LocalInstanceSpec,
    pub rendered: &'a Rendered,
    /// The server `Deployment`'s image: the target once migrated, the
    /// running one while its migration waits; `None`: no `Deployment` yet
    /// (a new instance waiting for its first migration).
    pub server_image: Option<&'a str>,
    /// The image whose migration `Job` runs, while it is pending.
    pub migration: Option<&'a str>,
    pub web_image: &'a str,
    /// The generated `Secret`s that don't exist yet: name → fresh key.
    pub generated: &'a BTreeMap<String, String>,
    /// The sinks' secrets: file name under [`paths::SINK_SECRETS_DIR`] →
    /// value.
    pub sinks: &'a BTreeMap<String, String>,
    /// The claim asks for more than it has: applied again (it can only
    /// grow), not created once.
    pub grow_claim: bool,
    /// The operator's pods: let in by the server's `NetworkPolicy` when it
    /// restricts its ingress (it reads `/healthz`).
    pub operator: &'a OperatorPod,
}

/// The labels of every object of an instance: `managed-by` and `instance`
/// hand an existing object over to it (the kit's `is_ours`).
pub fn labels(instance: &str, component: &str) -> BTreeMap<String, String> {
    let mut labels = selector(instance, component);
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        api::MANAGED_BY.to_string(),
    );
    labels
}

/// The labels selecting one component's pods.
pub fn selector(instance: &str, component: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), "notify".to_string()),
        (
            "app.kubernetes.io/instance".to_string(),
            instance.to_string(),
        ),
        (
            "app.kubernetes.io/component".to_string(),
            component.to_string(),
        ),
    ])
}

/// `repository:tag`, the spec's or the defaults.
pub fn image(spec: &api::local_instance::Image, default_image: &str) -> String {
    let (repository, tag) = default_image
        .rsplit_once(':')
        .filter(|(_, tag)| !tag.contains('/'))
        .unwrap_or((default_image, api::notify::VERSION));
    format!(
        "{}:{}",
        spec.repository.as_deref().unwrap_or(repository),
        spec.tag.as_deref().unwrap_or(tag)
    )
}

/// The tag of an image reference.
pub fn tag(image: &str) -> &str {
    let name = image.split('@').next().unwrap_or(image);
    match name.rsplit_once(':') {
        Some((_, tag)) if !tag.contains('/') => tag,
        _ => "latest",
    }
}

/// The migration `Job` of `image`: `<name>-migrate-<hash8>`.
pub fn migrate_job(name: &str, image: &str) -> String {
    hashed_name(&Names(name).migrate_prefix(), image.as_bytes())
}

fn meta(input: &WorkloadInput<'_>, name: String, component: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name),
        namespace: Some(input.namespace.into()),
        labels: Some(labels(input.name, component)),
        ..Default::default()
    }
}

fn some_map(map: &BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
    (!map.is_empty()).then(|| map.clone())
}

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.into(),
        mount_path: path.into(),
        read_only: Some(read_only),
        ..Default::default()
    }
}

fn http_probe(path: &str, period: i32, failures: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::String("http".into()),
            ..Default::default()
        }),
        period_seconds: Some(period),
        failure_threshold: Some(failures),
        ..Default::default()
    }
}

fn http_port(port: i32) -> ContainerPort {
    ContainerPort {
        name: Some("http".into()),
        container_port: port,
        protocol: Some("TCP".into()),
        ..Default::default()
    }
}

fn resources(cpu: &str, memory: &str, limit: &str) -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".into(), Quantity(cpu.into())),
            ("memory".into(), Quantity(memory.into())),
        ])),
        limits: Some(BTreeMap::from([("memory".into(), Quantity(limit.into()))])),
        ..Default::default()
    }
}

fn value(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

/// A `Secret` the operator writes, labelled so its pods may read it.
fn secret(
    input: &WorkloadInput<'_>,
    name: String,
    component: &str,
    data: BTreeMap<String, String>,
) -> Secret {
    let mut metadata = meta(input, name, component);
    metadata
        .labels
        .get_or_insert_default()
        .insert(api::CREDENTIAL_LABEL.into(), "true".into());
    Secret {
        metadata,
        data: Some(
            data.into_iter()
                .map(|(k, v)| (k, ByteString(v.into_bytes())))
                .collect(),
        ),
        ..Default::default()
    }
}

/// The server's variables: its file, no migration at boot, then the
/// secrets render chose (`WNK_*` from `Secret`s) and the plain variables
/// built from them.
fn server_env(input: &WorkloadInput<'_>) -> Vec<EnvVar> {
    let mut env = vec![
        value("WNK_CONFIG", paths::CONFIG_FILE),
        // The migration `Job` migrates; a pod never does (also in the file).
        value("WNK_DATABASE__MIGRATE_ON_START", "false"),
    ];
    for e in &input.rendered.env {
        env.push(EnvVar {
            name: e.name.clone(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: e.source.name.clone(),
                    key: e.source.key.clone(),
                    optional: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    for (name, text) in &input.rendered.literal_env {
        env.push(value(name, text));
    }
    env
}

/// The server's volumes and mounts: the file's `ConfigMap` (a directory,
/// never a `subPath`: the kubelet swaps a symlink, which Notify's file
/// watch sees), the files render chose, the filesystem store.
fn server_volumes(input: &WorkloadInput<'_>) -> (Vec<Volume>, Vec<VolumeMount>) {
    let names = Names(input.name);
    let mut volumes = vec![Volume {
        name: "config".into(),
        config_map: Some(ConfigMapVolumeSource {
            name: names.config(),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut mounts = vec![mount("config", paths::CONFIG_DIR, true)];
    let mut sinks = false;
    for (i, file) in input.rendered.files.iter().enumerate() {
        if file.path.starts_with(paths::SINK_SECRETS_DIR) {
            sinks = true;
            continue;
        }
        let Some((dir, file_name)) = file.path.rsplit_once('/') else {
            continue;
        };
        let volume = format!("secret-{i}");
        volumes.push(Volume {
            name: volume.clone(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(file.source.name.clone()),
                items: Some(vec![KeyToPath {
                    key: file.source.key.clone(),
                    path: file_name.into(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount(&volume, dir, true));
    }
    if sinks {
        volumes.push(Volume {
            name: "sinks".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(names.sinks()),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount("sinks", paths::SINK_SECRETS_DIR, true));
    }
    if input.spec.attachments.filesystem.is_some() {
        volumes.push(Volume {
            name: "attachments".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: names.attachments(),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(mount("attachments", paths::ATTACHMENTS_DIR, false));
    }
    (volumes, mounts)
}

/// The server's pod: `notify-server` (with `args`) on `image`.
fn server_pod(
    input: &WorkloadInput<'_>,
    image: &str,
    component: &str,
    args: Vec<String>,
) -> PodTemplateSpec {
    let spec = input.spec;
    let (volumes, mounts) = server_volumes(input);
    let mut container = Container {
        name: component.into(),
        image: Some(image.into()),
        image_pull_policy: Some(spec.image.pull_policy.unwrap_or_default().as_str().into()),
        args: (!args.is_empty()).then_some(args),
        env: Some(server_env(input)),
        resources: Some(
            spec.resources
                .clone()
                .unwrap_or_else(|| resources("100m", "128Mi", "512Mi")),
        ),
        security_context: Some(restricted_container_security()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    let mut labels = selector(input.name, component);
    labels.insert(SERVER_POD_LABEL.into(), "true".into());
    let mut annotations = spec.pod_annotations.clone();
    if component == "server" {
        container.ports = Some(vec![http_port(i32::from(paths::PORT))]);
        // Migrations and the boot reconcile run before the listener binds.
        container.startup_probe = Some(http_probe("/healthz", 5, 60));
        container.liveness_probe = Some(http_probe("/healthz", 10, 3));
        container.readiness_probe = Some(http_probe("/readyz", 10, 3));
        // Distroless: the kubelet sleeps while the endpoints drain.
        container.lifecycle = Some(Lifecycle {
            pre_stop: Some(LifecycleHandler {
                sleep: Some(SleepAction { seconds: 5 }),
                ..Default::default()
            }),
            ..Default::default()
        });
        // What rolls the pods: the boot configuration, the image. The
        // config digest is never here — a hot change is reloaded.
        annotations.insert(
            api::annotations::BOOT_HASH.into(),
            input.rendered.boot_hash.clone(),
        );
        annotations.insert(IMAGE_ANNOTATION.into(), image.into());
    }
    PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels),
            annotations: some_map(&annotations),
            ..Default::default()
        }),
        spec: Some(PodSpec {
            service_account_name: Some(Names(input.name).server()),
            automount_service_account_token: Some(false),
            image_pull_secrets: (!spec.image_pull_secrets.is_empty())
                .then(|| spec.image_pull_secrets.clone()),
            security_context: Some(restricted_pod_security(NOTIFY_UID)),
            node_selector: some_map(&spec.node_selector),
            tolerations: (!spec.tolerations.is_empty()).then(|| spec.tolerations.clone()),
            affinity: spec.affinity.clone(),
            restart_policy: (component != "server").then(|| "Never".into()),
            volumes: Some(volumes),
            containers: vec![container],
            ..Default::default()
        }),
    }
}

fn server_deployment(input: &WorkloadInput<'_>, image: &str) -> Deployment {
    // A filesystem store is a single-writer claim: a surge pod would wait
    // for it, the pods are recreated instead.
    let strategy = if input.spec.attachments.filesystem.is_some() {
        DeploymentStrategy {
            type_: Some("Recreate".into()),
            ..Default::default()
        }
    } else {
        DeploymentStrategy {
            type_: Some("RollingUpdate".into()),
            rolling_update: Some(RollingUpdateDeployment {
                max_unavailable: Some(IntOrString::Int(0)),
                max_surge: Some(IntOrString::Int(1)),
            }),
        }
    };
    Deployment {
        metadata: meta(input, Names(input.name).server(), "server"),
        spec: Some(DeploymentSpec {
            replicas: Some(input.spec.replicas),
            selector: LabelSelector {
                match_labels: Some(selector(input.name, "server")),
                ..Default::default()
            },
            strategy: Some(strategy),
            progress_deadline_seconds: Some(600),
            template: server_pod(input, image, "server", Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn migrate_job_object(input: &WorkloadInput<'_>, image: &str) -> Job {
    Job {
        metadata: meta(input, migrate_job(input.name, image), "migrate"),
        spec: Some(JobSpec {
            backoff_limit: Some(3),
            active_deadline_seconds: Some(1800),
            template: server_pod(input, image, "migrate", vec!["migrate".into()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn service(input: &WorkloadInput<'_>, name: String, component: &str, port: i32) -> Service {
    Service {
        metadata: meta(input, name, component),
        spec: Some(ServiceSpec {
            selector: Some(selector(input.name, component)),
            ports: Some(vec![ServicePort {
                name: Some("http".into()),
                port,
                target_port: Some(IntOrString::String("http".into())),
                protocol: Some("TCP".into()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The origin (`scheme://host[:port]`) of a URL, `None` when it doesn't
/// parse.
pub fn origin(text: &str) -> Option<String> {
    let url = url::Url::parse(text).ok()?;
    url.host_str()?;
    Some(url.origin().ascii_serialization())
}

/// The port a URL's connections go to: its own, or its scheme's.
fn port_of(text: &str) -> Option<u16> {
    let url = url::Url::parse(text).ok()?;
    url.port().or(match url.scheme() {
        "https" => Some(443),
        "http" => Some(80),
        "smtps" => Some(465),
        "smtp" => Some(25),
        _ => None,
    })
}

/// Where the browser reaches the API: its `Ingress` host, or (no
/// `Ingress`) the `Service`, for a UI reached through a port-forward.
pub fn api_origin(input: &WorkloadInput<'_>) -> String {
    match &input.spec.ingress {
        Some(ingress) => format!("https://{}", ingress.api.host),
        None => format!(
            "http://{}.{}.svc:{}",
            Names(input.name).server(),
            input.namespace,
            paths::PORT
        ),
    }
}

fn web_deployment(input: &WorkloadInput<'_>) -> Deployment {
    let spec = input.spec;
    // ADR-0021: rendered into /env.js and the CSP at boot.
    let mut env = vec![
        value("WNK_API_BASE_URL", &api_origin(input)),
        // The issuer as the server checks it: the UI discovers it (its
        // origin is derived for the CSP by the image).
        value("WNK_OIDC_ISSUER", &spec.oidc.issuer),
        value("WNK_OIDC_CLIENT_ID", &spec.oidc.client_id),
    ];
    let attachments = match (&spec.attachments.s3, &spec.attachments.filesystem) {
        (Some(s3), _) => origin(&s3.endpoint),
        (None, Some(fs)) => origin(&fs.public_base_url),
        (None, None) => None,
    };
    if let Some(attachments) = attachments {
        env.push(value("WNK_ATTACHMENT_ORIGIN", &attachments));
    }
    let writable = [
        ("html", "/usr/share/nginx/html"),
        ("nginx-conf", "/etc/nginx/conf.d"),
        ("nginx-cache", "/var/cache/nginx"),
        ("nginx-run", "/var/run"),
        ("tmp", "/tmp"),
    ];
    let volumes = writable
        .iter()
        .map(|(name, _)| Volume {
            name: (*name).into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        })
        .collect();
    let mounts = writable
        .iter()
        .map(|(name, path)| mount(name, path, false))
        .collect();
    // The image writes /env.js into its web root at boot: the root is
    // seeded into a writable volume first, the container stays read-only.
    let mut seed_security = restricted_container_security();
    seed_security.read_only_root_filesystem = Some(false);
    let seed = Container {
        name: "seed-html".into(),
        image: Some(input.web_image.into()),
        image_pull_policy: Some(
            spec.web
                .image
                .pull_policy
                .unwrap_or_default()
                .as_str()
                .into(),
        ),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "cp -a /usr/share/nginx/html/. /seeded-html/".into(),
        ]),
        security_context: Some(seed_security),
        volume_mounts: Some(vec![mount("html", "/seeded-html", false)]),
        ..Default::default()
    };
    let container = Container {
        name: "web".into(),
        image: Some(input.web_image.into()),
        image_pull_policy: seed.image_pull_policy.clone(),
        ports: Some(vec![http_port(WEB_PORT)]),
        env: Some(env),
        liveness_probe: Some(http_probe("/healthz", 10, 3)),
        readiness_probe: Some(http_probe("/healthz", 10, 3)),
        resources: Some(
            spec.web
                .resources
                .clone()
                .unwrap_or_else(|| resources("10m", "16Mi", "64Mi")),
        ),
        security_context: Some(restricted_container_security()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    Deployment {
        metadata: meta(input, Names(input.name).web(), "web"),
        spec: Some(DeploymentSpec {
            replicas: Some(spec.web.replicas),
            selector: LabelSelector {
                match_labels: Some(selector(input.name, "web")),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(selector(input.name, "web")),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(Names(input.name).server()),
                    automount_service_account_token: Some(false),
                    image_pull_secrets: (!spec.image_pull_secrets.is_empty())
                        .then(|| spec.image_pull_secrets.clone()),
                    security_context: Some(restricted_pod_security(WEB_UID)),
                    node_selector: some_map(&spec.node_selector),
                    tolerations: (!spec.tolerations.is_empty()).then(|| spec.tolerations.clone()),
                    init_containers: Some(vec![seed]),
                    containers: vec![container],
                    volumes: Some(volumes),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn ingress(
    input: &WorkloadInput<'_>,
    name: String,
    component: &str,
    host: &api::local_instance::IngressHost,
    path: &str,
) -> Ingress {
    let spec = input.spec.ingress.clone().unwrap_or_default();
    let mut metadata = meta(input, name.clone(), component);
    metadata.annotations = some_map(&spec.annotations);
    Ingress {
        metadata,
        spec: Some(IngressSpec {
            ingress_class_name: spec.class_name.clone(),
            tls: host.tls_secret_name.as_ref().map(|secret| {
                vec![IngressTLS {
                    hosts: Some(vec![host.host.clone()]),
                    secret_name: Some(secret.clone()),
                }]
            }),
            rules: Some(vec![IngressRule {
                host: Some(host.host.clone()),
                http: Some(HTTPIngressRuleValue {
                    paths: vec![HTTPIngressPath {
                        path: Some(path.into()),
                        path_type: "Prefix".into(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name,
                                port: Some(ServiceBackendPort {
                                    name: Some("http".into()),
                                    ..Default::default()
                                }),
                            }),
                            ..Default::default()
                        },
                    }],
                }),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The ports the server connects to besides DNS: HTTPS, Postgres, and
/// those of the endpoints the spec names (object store, SMTP relay,
/// collector, identity provider).
pub fn egress_ports(spec: &LocalInstanceSpec) -> Vec<u16> {
    let mut ports = std::collections::BTreeSet::from([443, 5432]);
    let urls = [
        spec.attachments.s3.as_ref().map(|s| s.endpoint.as_str()),
        spec.smtp.as_ref().map(|s| s.url.as_str()),
        spec.telemetry.as_ref().map(|t| t.otlp_endpoint.as_str()),
        Some(spec.oidc.issuer.as_str()),
    ];
    ports.extend(urls.into_iter().flatten().filter_map(port_of));
    ports.into_iter().collect()
}

fn network_policies(input: &WorkloadInput<'_>) -> Vec<OwnedObject> {
    let spec = &input.spec.network_policy;
    let names = Names(input.name);
    let mut server_pods = BTreeMap::from([
        ("app.kubernetes.io/instance".to_string(), input.name.into()),
        (SERVER_POD_LABEL.to_string(), "true".to_string()),
    ]);
    server_pods.insert("app.kubernetes.io/name".into(), "notify".into());
    let mut server = network_policy(
        meta(input, names.server(), "server"),
        &PolicyRules {
            pod_labels: server_pods,
            port: "http",
            ingress_from: &spec.ingress_from,
            egress_to: &spec.egress_to,
            operator: Some(input.operator),
        },
    );
    if let Some(egress) = server.spec.as_mut().and_then(|s| s.egress.as_mut()) {
        egress.push(NetworkPolicyEgressRule {
            to: None,
            ports: Some(
                egress_ports(input.spec)
                    .into_iter()
                    .map(|port| NetworkPolicyPort {
                        port: Some(IntOrString::Int(i32::from(port))),
                        protocol: Some("TCP".into()),
                        ..Default::default()
                    })
                    .collect(),
            ),
        });
    }
    let mut out = vec![OwnedObject::of(&server, false)];
    if input.spec.web.enabled {
        // The UI is static: DNS only out.
        let web = network_policy(
            meta(input, names.web(), "web"),
            &PolicyRules {
                pod_labels: selector(input.name, "web"),
                port: "http",
                ingress_from: &spec.ingress_from,
                egress_to: &[],
                operator: None,
            },
        );
        out.push(OwnedObject::of(&web, false));
    }
    out
}

/// Every object of the instance, in the order they are applied (what a pod
/// mounts before the pods). The `ServiceMonitor` is apart
/// ([`service_monitor`]): its kind may not exist.
pub fn objects(input: &WorkloadInput<'_>) -> Vec<OwnedObject> {
    let spec = input.spec;
    let names = Names(input.name);
    let mut out = Vec::new();
    for (name, key) in input.generated {
        let data = BTreeMap::from([(paths::GENERATED_KEY.to_string(), key.clone())]);
        out.push(OwnedObject::of(
            &secret(input, name.clone(), "key", data),
            true,
        ));
    }
    if !input.sinks.is_empty() {
        out.push(OwnedObject::of(
            &secret(input, names.sinks(), "sinks", input.sinks.clone()),
            false,
        ));
    }
    out.push(OwnedObject::of(
        &ConfigMap {
            metadata: meta(input, names.config(), "config"),
            data: Some(BTreeMap::from([(
                paths::CONFIG_KEY.to_string(),
                input.rendered.text.clone(),
            )])),
            ..Default::default()
        },
        false,
    ));
    out.push(OwnedObject::of(
        &ServiceAccount {
            metadata: meta(input, names.server(), "server"),
            automount_service_account_token: Some(false),
            ..Default::default()
        },
        false,
    ));
    if let Some(fs) = &spec.attachments.filesystem {
        out.push(OwnedObject::of(
            &PersistentVolumeClaim {
                metadata: meta(input, names.attachments(), "attachments"),
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".into()]),
                    storage_class_name: fs.storage_class_name.clone(),
                    resources: Some(VolumeResourceRequirements {
                        requests: Some(BTreeMap::from([("storage".into(), fs.size.clone())])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            !input.grow_claim,
        ));
    }
    out.push(OwnedObject::of(
        &service(input, names.server(), "server", i32::from(paths::PORT)),
        false,
    ));
    if let Some(image) = input.migration {
        // A Job's pod template is immutable: one per image, created once.
        out.push(OwnedObject::of(&migrate_job_object(input, image), true));
    }
    if let Some(image) = input.server_image {
        out.push(OwnedObject::of(&server_deployment(input, image), false));
        if spec.replicas > 1 {
            out.push(OwnedObject::of(
                &PodDisruptionBudget {
                    metadata: meta(input, names.server(), "server"),
                    spec: Some(PodDisruptionBudgetSpec {
                        min_available: Some(IntOrString::Int(1)),
                        selector: Some(LabelSelector {
                            match_labels: Some(selector(input.name, "server")),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                false,
            ));
        }
    }
    out.extend(network_policies(input));
    if spec.web.enabled {
        out.push(OwnedObject::of(&web_deployment(input), false));
        out.push(OwnedObject::of(
            &service(input, names.web(), "web", WEB_PORT),
            false,
        ));
    }
    if let Some(ingress_spec) = &spec.ingress {
        // The API only: never /healthz, /readyz or /metrics.
        out.push(OwnedObject::of(
            &ingress(
                input,
                names.server(),
                "server",
                &ingress_spec.api,
                "/api/v1",
            ),
            false,
        ));
        if let Some(web) = ingress_spec.web.as_ref().filter(|_| spec.web.enabled) {
            out.push(OwnedObject::of(
                &ingress(input, names.web(), "web", web, "/"),
                false,
            ));
        }
    }
    out
}

/// The `ServiceMonitor` (`monitoring.coreos.com/v1`), when enabled.
pub fn service_monitor(input: &WorkloadInput<'_>) -> Option<OwnedObject> {
    let monitor = &input.spec.service_monitor;
    if !monitor.enabled {
        return None;
    }
    let mut labels = labels(input.name, "server");
    labels.extend(monitor.labels.clone());
    let name = Names(input.name).server();
    Some(OwnedObject {
        api_version: "monitoring.coreos.com/v1".into(),
        kind: "ServiceMonitor".into(),
        name: name.clone(),
        manifest: serde_json::json!({
            "apiVersion": "monitoring.coreos.com/v1",
            "kind": "ServiceMonitor",
            "metadata": {"name": name, "namespace": input.namespace, "labels": labels},
            "spec": {
                "selector": {"matchLabels": selector(input.name, "server")},
                "endpoints": [{"port": "http", "path": "/metrics"}],
            },
        }),
        create_only: false,
    })
}

/// The objects the spec no longer wants, which the operator removes:
/// `(apiVersion, kind, name)`. The claim and the generated keys are never
/// removed (data).
pub fn removed(input: &WorkloadInput<'_>) -> Vec<(&'static str, &'static str, String)> {
    let spec = input.spec;
    let names = Names(input.name);
    let mut out = Vec::new();
    if spec.replicas < 2 || input.server_image.is_none() {
        out.push(("policy/v1", "PodDisruptionBudget", names.server()));
    }
    if !spec.web.enabled {
        out.push(("apps/v1", "Deployment", names.web()));
        out.push(("v1", "Service", names.web()));
        out.push(("networking.k8s.io/v1", "NetworkPolicy", names.web()));
    }
    match &spec.ingress {
        None => {
            out.push(("networking.k8s.io/v1", "Ingress", names.server()));
            out.push(("networking.k8s.io/v1", "Ingress", names.web()));
        }
        Some(ingress) if ingress.web.is_none() || !spec.web.enabled => {
            out.push(("networking.k8s.io/v1", "Ingress", names.web()));
        }
        Some(_) => {}
    }
    if !spec.service_monitor.enabled {
        out.push(("monitoring.coreos.com/v1", "ServiceMonitor", names.server()));
    }
    if input.sinks.is_empty() {
        out.push(("v1", "Secret", names.sinks()));
    }
    out
}

/// A `Quantity` in bytes, for the suffixes a claim's size uses (`5Gi`,
/// `500M`, `1073741824`); `None` otherwise.
pub fn quantity_bytes(quantity: &str) -> Option<u128> {
    let quantity = quantity.trim();
    let digits = quantity
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(quantity.len());
    let (number, suffix) = quantity.split_at(digits);
    let number: u128 = number.parse().ok()?;
    let factor: u128 = match suffix {
        "" => 1,
        "k" => 1_000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        "P" => 1_000_000_000_000_000,
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        "Pi" => 1 << 50,
        _ => return None,
    };
    number.checked_mul(factor)
}

/// Whether the claim must be applied again: the spec asks for more than
/// it has (`current`, `None`: absent or unreadable).
pub fn claim_grows(spec: &LocalInstanceSpec, current: Option<&str>) -> bool {
    let (Some(fs), Some(current)) = (&spec.attachments.filesystem, current) else {
        return false;
    };
    match (quantity_bytes(&fs.size.0), quantity_bytes(current)) {
        (Some(wanted), Some(has)) => wanted > has,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
