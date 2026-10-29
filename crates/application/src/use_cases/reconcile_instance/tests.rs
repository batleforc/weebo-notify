use api::status::RenderState;
use serde_json::json;
use weebo_kit_application::test_support::FakeOwnedObjects;

use super::*;
use crate::objects::{ObjectSpec, ObjectView};
use crate::ports::JobState;
use crate::test_support::{FakeCredentials, FakeDirectory, FakeJobs, FakeKeys, FakeLedger};

const IMAGE: &str = "ghcr.io/batleforc/weebo-notify:0.3.0";

fn spec(extra: serde_json::Value) -> LocalInstanceSpec {
    let mut base = json!({
        "database": {"urlSecretRef": {"name": "db", "key": "url"}},
        "attachments": {"s3": {"endpoint": "http://rustfs:9000", "bucket": "notify",
                               "credentialsSecretRef": {"name": "s3"}}},
        "oidc": {"issuer": "https://id.example", "clientId": "notify"},
    });
    if let (Some(base), serde_json::Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
    serde_json::from_value(base).unwrap()
}

fn channel(name: &str) -> ObjectView {
    ObjectView {
        namespace: "team".into(),
        name: name.into(),
        created: Some("2026-01-01T00:00:00Z".parse().unwrap()),
        generation: Some(1),
        spec: ObjectSpec::Channel(
            serde_json::from_value(json!({
                "sinks": [{"type": "webhook", "url": "https://hooks.example.com",
                           "secretRef": {"name": "hook", "key": "secret"}}],
            }))
            .unwrap(),
        ),
        deleting: false,
    }
}

/// An instance `notify/main` and every port in memory.
struct Harness {
    owner: Owner,
    spec: LocalInstanceSpec,
    annotations: BTreeMap<String, String>,
    credentials: FakeCredentials,
    directory: FakeDirectory,
    objects: FakeOwnedObjects,
    ledger: FakeLedger,
    jobs: FakeJobs,
    keys: FakeKeys,
    operator: OperatorPod,
}

impl Harness {
    fn new(spec: LocalInstanceSpec, views: Vec<ObjectView>) -> Self {
        let credentials = FakeCredentials::default();
        credentials.insert("notify", "db", &[("url", "postgres://u:p@db/notify")]);
        credentials.insert(
            "notify",
            "s3",
            &[("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "s")],
        );
        credentials.insert("team", "hook", &[("secret", "s3cr3t")]);
        Self {
            owner: Owner {
                namespace: "notify".into(),
                name: "main".into(),
                uid: "uid-1".into(),
            },
            spec,
            annotations: BTreeMap::new(),
            credentials,
            directory: FakeDirectory::with(views),
            objects: FakeOwnedObjects::default(),
            ledger: FakeLedger::default(),
            jobs: FakeJobs::default(),
            keys: FakeKeys::default(),
            operator: OperatorPod::named("weebo-notify"),
        }
    }

    async fn pass(&self, status: &InstanceStatus) -> InstanceReport {
        reconcile_instance(
            InstancePass {
                owner: &self.owner,
                spec: &self.spec,
                annotations: &self.annotations,
                status,
                default_image: IMAGE,
                default_web_image: "ghcr.io/batleforc/weebo-notify-web:0.3.0",
                operator: &self.operator,
            },
            &InstancePorts {
                objects: &self.objects,
                credentials: &self.credentials,
                ledger: &self.ledger,
                directory: &self.directory,
                jobs: &self.jobs,
                keys: &self.keys,
            },
        )
        .await
    }

    /// The first pass, the migration Job succeeding, the second pass.
    async fn migrated(&self) -> InstanceReport {
        let first = self.pass(&InstanceStatus::default()).await;
        self.jobs.set(
            &workload::migrate_job("main", IMAGE),
            JobState {
                succeeded: true,
                ..Default::default()
            },
        );
        self.pass(&first.status).await
    }

    fn server_image(&self) -> Option<String> {
        let deployment = self.objects.object("Deployment", "main")?;
        deployment.manifest["spec"]["template"]["spec"]["containers"][0]["image"]
            .as_str()
            .map(String::from)
    }
}

#[tokio::test]
async fn a_new_instance_is_migrated_before_its_server_starts() {
    let h = Harness::new(spec(json!({})), vec![channel("alerts")]);
    let first = h.pass(&InstanceStatus::default()).await;
    assert_eq!(first.outcome.reason(), ReasonCode::Migrating);
    let job = workload::migrate_job("main", IMAGE);
    assert!(h.objects.object("Job", &job).is_some());
    assert!(h.server_image().is_none(), "no server before its schema");
    // The file is written all the same, and recorded.
    let map = h.objects.object("ConfigMap", "main-config").unwrap();
    let text = map.manifest["data"]["config.toml"].as_str().unwrap();
    assert!(text.contains("id = \"alerts\""), "{text}");
    assert!(!text.contains("s3cr3t"));
    assert_eq!(
        h.ledger.last().unwrap()["configDigest"],
        json!(first.status.config_digest)
    );
    assert_eq!(
        first.status.objects["NotifyChannel/team/alerts"].state,
        RenderState::Rendered
    );

    h.jobs.set(
        &job,
        JobState {
            succeeded: true,
            ..Default::default()
        },
    );
    let second = h.pass(&first.status).await;
    assert_eq!(second.outcome.reason(), ReasonCode::Reconciled);
    assert_eq!(second.status.migrated_version.as_deref(), Some("0.3.0"));
    assert_eq!(second.status.version.as_deref(), Some("0.3.0"));
    assert_eq!(second.status.available_replicas, Some(2));
    assert_eq!(h.server_image().as_deref(), Some(IMAGE));
    assert!(
        second
            .events
            .iter()
            .any(|(r, _)| *r == ReasonCode::Migrated)
    );
    assert!(
        second
            .outcome
            .advisories
            .iter()
            .any(|a| a.reason == ReasonCode::Migrated)
    );
    assert_eq!(
        second.status.endpoint.as_deref(),
        Some("http://main.notify.svc:8080")
    );
    // The sinks' secret copied next to the pods.
    assert_eq!(
        h.objects
            .secret(
                "main-sinks",
                &render::sink_entry(&render::SecretSource {
                    namespace: "team".into(),
                    name: "hook".into(),
                    key: "secret".into(),
                })
            )
            .as_deref(),
        Some("s3cr3t")
    );
    // Migrated once: a third pass runs no Job and reads none.
    let third = h.pass(&second.status).await;
    assert_eq!(third.outcome.reason(), ReasonCode::Reconciled);
    assert!(third.events.is_empty());
}

#[tokio::test]
async fn a_failed_migration_keeps_the_pods_on_their_version() {
    let mut h = Harness::new(spec(json!({})), Vec::new());
    let migrated = h.migrated().await;
    // An approved 0.4.0: its Job fails.
    h.spec.image.tag = Some("0.4.0".into());
    h.annotations
        .insert(api::annotations::APPROVED_UPGRADE.into(), "0.4.0".into());
    let target = "ghcr.io/batleforc/weebo-notify:0.4.0";
    h.jobs.set(
        &workload::migrate_job("main", target),
        JobState {
            failed: Some("BackoffLimitExceeded".into()),
            ..Default::default()
        },
    );
    let report = h.pass(&migrated.status).await;
    assert_eq!(report.outcome.reason(), ReasonCode::MigrationFailed);
    assert_eq!(h.server_image().as_deref(), Some(IMAGE));
    assert_eq!(report.status.migrated_version.as_deref(), Some("0.3.0"));
    // The old Job is kept (its log), next to the new one.
    assert_eq!(h.objects.names("Job").len(), 2);
}

#[tokio::test]
async fn an_unsupported_or_older_tag_needs_the_approval() {
    let mut h = Harness::new(spec(json!({"image": {"tag": "9.9.9"}})), Vec::new());
    let report = h.pass(&InstanceStatus::default()).await;
    assert_eq!(report.outcome.reason(), ReasonCode::UnsupportedVersion);
    assert!(h.objects.names("ConfigMap").is_empty(), "nothing written");
    h.annotations
        .insert(api::annotations::APPROVED_UPGRADE.into(), "9.9.9".into());
    assert_eq!(
        h.pass(&InstanceStatus::default()).await.outcome.reason(),
        ReasonCode::Migrating
    );
    // Below the migrated version.
    let h = Harness::new(spec(json!({})), Vec::new());
    let status = InstanceStatus {
        migrated_version: Some("0.4.0".into()),
        ..Default::default()
    };
    assert_eq!(
        h.pass(&status).await.outcome.reason(),
        ReasonCode::UnsupportedVersion
    );
}

#[tokio::test]
async fn generated_keys_are_made_once() {
    let h = Harness::new(spec(json!({})), Vec::new());
    h.migrated().await;
    let fingerprint = h.objects.object("Secret", "main-fingerprint-key").unwrap();
    assert!(fingerprint.create_only);
    let made = h.keys.made();
    assert_eq!(made, 1);
    h.pass(&InstanceStatus::default()).await;
    assert_eq!(h.keys.made(), made, "no new key for an existing Secret");
}

#[tokio::test]
async fn pods_still_rolling_are_not_ready() {
    let h = Harness::new(spec(json!({})), Vec::new());
    let migrated = h.migrated().await;
    h.objects.hold_rollout(true);
    let report = h.pass(&migrated.status).await;
    assert_eq!(report.outcome.reason(), ReasonCode::WorkloadNotReady);
    assert_eq!(report.status.available_replicas, Some(0));
}

#[tokio::test]
async fn what_the_spec_no_longer_wants_is_removed() {
    let mut h = Harness::new(spec(json!({})), Vec::new());
    h.migrated().await;
    assert!(h.objects.object("Deployment", "main-web").is_some());
    assert!(h.objects.object("PodDisruptionBudget", "main").is_some());
    h.spec.web.enabled = false;
    h.spec.replicas = 1;
    h.pass(&InstanceStatus {
        migrated_version: Some("0.3.0".into()),
        ..Default::default()
    })
    .await;
    assert!(h.objects.object("Deployment", "main-web").is_none());
    assert!(h.objects.object("PodDisruptionBudget", "main").is_none());
    let log = h.objects.log();
    assert!(log.contains(&"Deployment/main-web".to_string()), "{log:?}");
}

#[tokio::test]
async fn a_secret_missing_stops_the_instance_or_leaves_its_object_out() {
    let h = Harness::new(spec(json!({})), vec![channel("alerts")]);
    let none = Harness {
        credentials: FakeCredentials::default(),
        ..Harness::new(spec(json!({})), vec![channel("alerts")])
    };
    let report = none.pass(&InstanceStatus::default()).await;
    assert_eq!(report.outcome.reason(), ReasonCode::SecretMissing);
    assert!(none.objects.object("ConfigMap", "main-config").is_none());

    h.credentials
        .insert_unlabelled("team", "hook", &[("secret", "s3cr3t")]);
    let report = h.pass(&InstanceStatus::default()).await;
    assert_eq!(
        report.status.objects["NotifyChannel/team/alerts"].state,
        RenderState::SecretNotAllowed
    );
    assert!(h.objects.object("Secret", "main-sinks").is_none());
}

#[tokio::test]
async fn an_object_gone_is_dropped_from_the_status_by_name() {
    let h = Harness::new(spec(json!({})), vec![channel("alerts")]);
    let first = h.pass(&InstanceStatus::default()).await;
    let gone = Harness::new(spec(json!({})), Vec::new());
    let second = gone.pass(&first.status).await;
    assert!(second.status.objects.is_empty());
    let fields = status_fields(&first.status, &second.status);
    assert_eq!(
        fields["objects"],
        json!({"NotifyChannel/team/alerts": null})
    );
}

#[tokio::test]
async fn an_owned_object_taken_is_a_conflict() {
    let h = Harness::new(spec(json!({})), Vec::new());
    h.objects.foreign("ConfigMap", "main-config");
    let report = h.pass(&InstanceStatus::default()).await;
    assert_eq!(report.outcome.reason(), ReasonCode::OwnedObjectConflict);
}
