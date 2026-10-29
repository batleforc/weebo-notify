//! What the envtest suites share: the operator's context as
//! `operator::main` builds it, on a client of the test's choosing — the
//! real adapters, but the pods' `/healthz` in memory (no kubelet runs a pod
//! in envtest) — and the fixtures.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers fail the test on purpose"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use adapters_inbound::controller::{self, Ctx, Settings, Shutdown};
use adapters_outbound::{
    KubeCredentials, KubeInstanceLedger, KubeJobs, KubeObjects, LocalInstanceDirectory, RandomKeys,
};
use application::test_support::FakeReplicas;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use kube::Client;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use kube::runtime::events::{Recorder, Reporter};
use weebo_kit_adapters::KubeOwnedObjects;
use weebo_kit_application::workload::OperatorPod;
use weebo_kit_runtime::{Health, WatchScope};

pub const TIMEOUT: Duration = Duration::from_secs(30);
pub const POLL: Duration = Duration::from_millis(200);

pub fn allowed_images() -> Vec<String> {
    vec![
        api::notify::SERVER_IMAGE.into(),
        api::notify::WEB_IMAGE.into(),
    ]
}

/// The context, and the pods' answers the test sets.
pub fn ctx(client: &Client, scope: WatchScope) -> (Arc<Ctx>, Arc<Health>, Arc<FakeReplicas>) {
    let health = Arc::new(Health::default());
    let watches = controller::spawn_watches(client, &scope, &health);
    let mut instances = LocalInstanceDirectory::new(client.clone()).with_scope(
        scope.watched_namespace().map(String::from),
        scope.selector().cloned(),
    );
    if let Some(shared) = watches.get::<api::LocalInstance>() {
        instances = instances.with_store(shared.store());
    }
    let instances = Arc::new(instances);
    let directory = Arc::new(
        KubeObjects::new(client.clone())
            .with_scope(
                scope.watched_namespace().map(String::from),
                scope.selector().cloned(),
            )
            .with_watches(watches.clone()),
    );
    let replicas = Arc::new(FakeReplicas::default());
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        instances: instances.clone(),
        attached: instances,
        objects: Arc::new(KubeOwnedObjects::new(
            client.clone(),
            adapters_outbound::workload_settings(allowed_images()),
        )),
        credentials: Arc::new(KubeCredentials::new(client.clone())),
        keys: Arc::new(RandomKeys),
        directory,
        ledger: Arc::new(KubeInstanceLedger::new(client.clone())),
        jobs: Arc::new(KubeJobs::new(client.clone())),
        health_checks: replicas.clone(),
        operator: OperatorPod::named("weebo-notify"),
        settings: Settings::default(),
        recorder: Recorder::new(
            client.clone(),
            Reporter {
                controller: controller::FIELD_MANAGER.into(),
                instance: None,
            },
        ),
        shutdown: Shutdown::never(),
        health: health.clone(),
        scope,
        watches,
    });
    (ctx, health, replicas)
}

pub async fn namespace(client: &Client, name: &str, labels: &[(&str, &str)]) {
    Api::<Namespace>::all(client.clone())
        .create(
            &PostParams::default(),
            &Namespace {
                metadata: ObjectMeta {
                    name: Some(name.into()),
                    labels: Some(
                        labels
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                    ),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

/// A `Secret` labelled `notify.weebo.io/credential: "true"`.
pub async fn credential(client: &Client, namespace: &str, name: &str, data: &[(&str, &str)]) {
    Api::<Secret>::namespaced(client.clone(), namespace)
        .create(
            &PostParams::default(),
            &Secret {
                metadata: ObjectMeta {
                    name: Some(name.into()),
                    labels: Some(BTreeMap::from([(
                        api::CREDENTIAL_LABEL.to_string(),
                        "true".to_string(),
                    )])),
                    ..Default::default()
                },
                data: Some(
                    data.iter()
                        .map(|(k, v)| (k.to_string(), ByteString(v.as_bytes().to_vec())))
                        .collect(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

/// The Secrets a minimal instance names.
pub async fn instance_secrets(client: &Client, namespace: &str) {
    credential(
        client,
        namespace,
        "db",
        &[("url", "postgres://u:p@db/notify")],
    )
    .await;
    credential(
        client,
        namespace,
        "s3",
        &[("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "s")],
    )
    .await;
}

pub fn instance_spec(extra: serde_json::Value) -> serde_json::Value {
    let mut spec = serde_json::json!({
        "database": {"urlSecretRef": {"name": "db", "key": "url"}},
        "attachments": {"s3": {"endpoint": "http://rustfs:9000", "bucket": "notify",
                               "credentialsSecretRef": {"name": "s3"}}},
        "oidc": {"issuer": "https://id.example", "clientId": "notify"},
    });
    if let (Some(spec), serde_json::Value::Object(extra)) = (spec.as_object_mut(), extra) {
        spec.extend(extra);
    }
    spec
}

/// A CR from its kind and spec.
pub fn cr<K: serde::de::DeserializeOwned>(
    kind: &str,
    name: &str,
    namespace: &str,
    spec: serde_json::Value,
) -> K {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "notify.weebo.io/v1alpha1",
        "kind": kind,
        "metadata": {"name": name, "namespace": namespace},
        "spec": spec,
    }))
    .unwrap()
}

/// `Ready`'s status and reason, once there is one.
pub fn ready(conditions: &[weebo_kit_api::Condition]) -> Option<(String, String)> {
    let c = weebo_kit_api::status::ready(conditions)?;
    Some((c.status.clone(), c.reason.clone()))
}

/// Wakes a CR's reconcile: an annotation (what the kit's predicates see).
pub async fn nudge<K>(api: &Api<K>, name: &str, n: u32)
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    api.patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({"metadata": {"annotations": {"test.weebo.io/nudge": n.to_string()}}})),
    )
    .await
    .unwrap();
}

/// Polls until `name` exists ([`testkit::polling::wait_for`] wants it
/// there from the start).
pub async fn appears<K>(api: &Api<K>, name: &str)
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    tokio::time::timeout(TIMEOUT, async {
        while api.get_opt(name).await.unwrap().is_none() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} never appeared"));
}
