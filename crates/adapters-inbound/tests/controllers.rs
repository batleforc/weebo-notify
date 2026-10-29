//! Integration tests: the instance and object controllers on a real
//! control plane (envtest), with the kit's `KubeOwnedObjects`. envtest has
//! no controller manager and no kubelet: the test plays them — it marks
//! the migration `Job` succeeded and the `Deployment` rolled out by
//! patching their status, and answers the pods' `/healthz` through
//! `FakeReplicas`.

#![expect(clippy::unwrap_used, reason = "test helpers fail the test on purpose")]

use std::time::Duration;

use adapters_inbound::controller;
use api::{Channel, LocalInstance, ServiceAccount};
use application::ports::{AppliedConfig, Replica};
use application::test_support::FakeReplicas;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::{Client, ResourceExt};
use serde_json::json;
use testkit::polling::{wait_for, wait_for_absence};
use weebo_kit_runtime::WatchScope;

mod common;

use common::{POLL, TIMEOUT, cr, instance_spec, nudge, ready};

const IMAGE: &str = "ghcr.io/batleforc/weebo-notify:0.3.0";

async fn reason_is<K>(
    api: &Api<K>,
    name: &str,
    reason: &str,
    conditions: fn(&K) -> Option<&[weebo_kit_api::Condition]>,
) where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let wanted = reason.to_string();
    wait_for(api, name, TIMEOUT, POLL, |o| {
        ready(conditions(o)?).filter(|(_, r)| *r == wanted)
    })
    .await;
}

fn instance_conditions(i: &LocalInstance) -> Option<&[weebo_kit_api::Condition]> {
    i.status.as_ref().map(|s| s.conditions.as_slice())
}

fn channel_conditions(c: &Channel) -> Option<&[weebo_kit_api::Condition]> {
    c.status.as_ref().map(|s| s.conditions.as_slice())
}

fn account_conditions(c: &ServiceAccount) -> Option<&[weebo_kit_api::Condition]> {
    c.status.as_ref().map(|s| s.conditions.as_slice())
}

/// The digest the instance last wrote, once it differs from `not`.
async fn digest(instances: &Api<LocalInstance>, not: Option<&str>) -> String {
    wait_for(instances, "main", TIMEOUT, POLL, |i| {
        i.status
            .as_ref()?
            .config_digest
            .clone()
            .filter(|d| Some(d.as_str()) != not)
    })
    .await
}

/// Both pods answer `/healthz` with `digest` (and `refused` accounts).
fn pods_report(replicas: &FakeReplicas, digest: &str, refused: &[&str]) {
    replicas.set(
        ["main-0", "main-1"]
            .map(|pod| Replica {
                pod: pod.into(),
                answer: Ok(AppliedConfig {
                    digest: Some(digest.into()),
                    refused_service_accounts: refused.iter().map(|r| r.to_string()).collect(),
                    ..Default::default()
                }),
            })
            .to_vec(),
    );
}

/// What the Job controller writes on a Job that succeeded.
async fn job_succeeded(client: &Client, name: &str) {
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    Api::<Job>::namespaced(client.clone(), "team")
        .patch_status(
            name,
            &PatchParams::default(),
            &Patch::Merge(json!({"status": {
                "startTime": now, "completionTime": now, "succeeded": 1,
                "conditions": [
                    {"type": "SuccessCriteriaMet", "status": "True", "lastTransitionTime": now, "lastProbeTime": now},
                    {"type": "Complete", "status": "True", "lastTransitionTime": now, "lastProbeTime": now},
                ],
            }})),
        )
        .await
        .unwrap();
}

/// What the Deployment controller writes once every replica is up.
async fn rolled_out(client: &Client, name: &str) {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "team");
    let deployment = deployments.get(name).await.unwrap();
    let replicas = deployment.spec.as_ref().unwrap().replicas.unwrap_or(1);
    deployments
        .patch_status(
            name,
            &PatchParams::default(),
            &Patch::Merge(json!({"status": {
                "observedGeneration": deployment.metadata.generation,
                "replicas": replicas, "updatedReplicas": replicas,
                "readyReplicas": replicas, "availableReplicas": replicas,
            }})),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_instance_and_its_objects_run_end_to_end() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .try_init()
        .unwrap_or_default();
    let cluster = testkit::envtest::start().await;
    let client = cluster.client();
    common::namespace(&client, "team", &[]).await;
    common::instance_secrets(&client, "team").await;
    let (ctx, health, replicas) = common::ctx(&client, WatchScope::cluster());
    tokio::spawn(controller::local_instance::run(client.clone(), ctx.clone()));
    tokio::spawn(controller::object::run::<Channel>(
        client.clone(),
        ctx.clone(),
    ));
    tokio::spawn(controller::object::run::<ServiceAccount>(
        client.clone(),
        ctx,
    ));
    let pp = PostParams::default();
    let instances: Api<LocalInstance> = Api::namespaced(client.clone(), "team");
    let channels: Api<Channel> = Api::namespaced(client.clone(), "team");
    let accounts: Api<ServiceAccount> = Api::namespaced(client.clone(), "team");

    // ── Created: migrated first, the file and the objects around it ──────
    instances
        .create(
            &pp,
            &cr(
                "NotifyLocalInstance",
                "main",
                "team",
                instance_spec(json!({})),
            ),
        )
        .await
        .unwrap();
    channels
        .create(
            &pp,
            &cr("NotifyChannel", "alerts", "team", json!({"name": "Alerts"})),
        )
        .await
        .unwrap();
    accounts
        .create(&pp, &cr("NotifyServiceAccount", "bot", "team", json!({})))
        .await
        .unwrap();
    reason_is(&instances, "main", "Migrating", instance_conditions).await;
    let maps: Api<ConfigMap> = Api::namespaced(client.clone(), "team");
    wait_for(&maps, "main-config", TIMEOUT, POLL, |m| {
        let text = m.data.as_ref()?.get("config.toml")?;
        (text.contains("id = \"alerts\"") && text.contains("id = \"bot\"")).then_some(())
    })
    .await;
    Api::<Service>::namespaced(client.clone(), "team")
        .get("main")
        .await
        .unwrap();
    Api::<NetworkPolicy>::namespaced(client.clone(), "team")
        .get("main")
        .await
        .unwrap();
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "team");
    assert!(
        deployments.get_opt("main").await.unwrap().is_none(),
        "no server before its migration"
    );
    let job = application::workload::migrate_job("main", IMAGE);
    let jobs: Api<Job> = Api::namespaced(client.clone(), "team");
    jobs.get(&job).await.unwrap();
    // No ServiceMonitor kind in envtest: said, not failed.
    let main = instances.get("main").await.unwrap();
    assert!(
        main.status
            .unwrap()
            .conditions
            .iter()
            .any(|c| c.type_ == "MonitoringUnavailable")
    );

    // ── The Job succeeds: the server on the image, then rolled out ───────
    job_succeeded(&client, &job).await;
    common::appears(&deployments, "main").await;
    let image = wait_for(&deployments, "main", TIMEOUT, POLL, |d| {
        d.spec.as_ref()?.template.spec.as_ref()?.containers[0]
            .image
            .clone()
    })
    .await;
    assert_eq!(image, IMAGE);
    rolled_out(&client, "main").await;
    reason_is(&instances, "main", "ConfigPropagating", instance_conditions).await;
    let migrated = instances.get("main").await.unwrap().status.unwrap();
    assert_eq!(migrated.migrated_version.as_deref(), Some("0.3.0"));

    // ── The pods report the file: applied, the objects Ready ─────────────
    let first = digest(&instances, None).await;
    pods_report(&replicas, &first, &[]);
    nudge(&instances, "main", 1).await;
    reason_is(&instances, "main", "Reconciled", instance_conditions).await;
    reason_is(&channels, "alerts", "Reconciled", channel_conditions).await;
    let status = instances.get("main").await.unwrap().status.unwrap();
    assert_eq!(
        status.applied_config_digest.as_deref(),
        Some(first.as_str())
    );
    assert_eq!(
        status.objects["NotifyChannel/team/alerts"].applied_generation,
        Some(1)
    );
    let alerts = channels.get("alerts").await.unwrap();
    assert_eq!(alerts.status.unwrap().id.as_deref(), Some("alerts"));

    // ── A channel deleted: held until a file without it is applied ───────
    channels
        .delete("alerts", &Default::default())
        .await
        .unwrap();
    let second = digest(&instances, Some(&first)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        channels.get_opt("alerts").await.unwrap().is_some(),
        "released before its removal was applied"
    );
    pods_report(&replicas, &second, &[]);
    nudge(&instances, "main", 2).await;
    wait_for_absence(&channels, "alerts", TIMEOUT, POLL).await;

    // ── A removal Notify's guard refuses: the finalizer stays ────────────
    accounts.delete("bot", &Default::default()).await.unwrap();
    let third = digest(&instances, Some(&second)).await;
    pods_report(&replicas, &third, &["bot"]);
    nudge(&instances, "main", 3).await;
    reason_is(&accounts, "bot", "RemovalRefused", account_conditions).await;
    let refused = instances.get("main").await.unwrap().status.unwrap();
    assert_eq!(refused.refused_removals.service_accounts, ["bot"]);
    // The guard lets it go on a later pass.
    pods_report(&replicas, &third, &[]);
    nudge(&instances, "main", 4).await;
    wait_for_absence(&accounts, "bot", Duration::from_secs(60), POLL).await;

    // ── The instance deleted: held while a live object is bound ──────────
    channels
        .create(&pp, &cr("NotifyChannel", "pages", "team", json!({})))
        .await
        .unwrap();
    wait_for(&channels, "pages", TIMEOUT, POLL, |c| {
        c.status.as_ref()?.instance.clone()
    })
    .await;
    instances.delete("main", &Default::default()).await.unwrap();
    reason_is(&instances, "main", "InstanceInUse", instance_conditions).await;
    let held = instances.get("main").await.unwrap();
    let message = &ready(&held.status.unwrap().conditions).unwrap();
    assert_eq!(message.0, "False");
    channels.delete("pages", &Default::default()).await.unwrap();
    wait_for_absence(&channels, "pages", Duration::from_secs(60), POLL).await;
    wait_for_absence(&instances, "main", Duration::from_secs(60), POLL).await;
    assert_eq!(health.live(), Ok(()));
}

#[tokio::test]
async fn the_oldest_object_keeps_an_id_across_namespaces() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .try_init()
        .unwrap_or_default();
    let cluster = testkit::envtest::start().await;
    let client = cluster.client();
    common::namespace(&client, "team", &[]).await;
    common::namespace(&client, "other", &[]).await;
    common::instance_secrets(&client, "team").await;
    let (ctx, health, _) = common::ctx(&client, WatchScope::cluster());
    tokio::spawn(controller::local_instance::run(client.clone(), ctx.clone()));
    tokio::spawn(controller::object::run::<Channel>(client.clone(), ctx));
    let pp = PostParams::default();
    Api::<LocalInstance>::namespaced(client.clone(), "team")
        .create(
            &pp,
            &cr(
                "NotifyLocalInstance",
                "main",
                "team",
                instance_spec(json!({"objectSelector": {"namespaces": {"from": "All"}}})),
            ),
        )
        .await
        .unwrap();
    let team: Api<Channel> = Api::namespaced(client.clone(), "team");
    let other: Api<Channel> = Api::namespaced(client.clone(), "other");
    team.create(&pp, &cr("NotifyChannel", "alerts", "team", json!({})))
        .await
        .unwrap();
    // Creation times are to the second: the second one strictly after.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    other
        .create(
            &pp,
            &cr(
                "NotifyChannel",
                "mine",
                "other",
                json!({"id": "alerts", "instanceRef": {"name": "main", "namespace": "team"}}),
            ),
        )
        .await
        .unwrap();
    reason_is(&other, "mine", "IdConflict", channel_conditions).await;
    let lost = other.get("mine").await.unwrap();
    let message = &lost.status.unwrap().conditions[0].message;
    assert!(message.contains("team/alerts"), "{message}");
    // The older one is rendered (waiting for pods that never come).
    reason_is(&team, "alerts", "ConfigPropagating", channel_conditions).await;
    let file = Api::<ConfigMap>::namespaced(client.clone(), "team")
        .get("main-config")
        .await
        .unwrap();
    let text = &file.data.unwrap()["config.toml"];
    assert_eq!(text.matches("id = \"alerts\"").count(), 1, "{text}");
    assert_eq!(health.live(), Ok(()));
    // Bound across namespaces, the other one names its instance.
    let bound = team.get("alerts").await.unwrap();
    assert!(
        bound
            .finalizers()
            .contains(&controller::FINALIZER.to_string())
    );
    assert_eq!(bound.status.unwrap().instance.unwrap().name, "main");
}
