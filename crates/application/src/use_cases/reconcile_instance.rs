//! A `NotifyLocalInstance`: its configuration, then (2b) its workload. One
//! pass:
//!
//! 1. the Secrets the spec references are checked (present, with the
//!    credential label): the pods read them, the operator never does;
//! 2. the objects bound to it are listed, the Secrets they reference
//!    checked;
//! 3. the file is rendered and checked by Notify's model (`InvalidSpec`:
//!    nothing written), each object's fate recorded in `status.objects`;
//! 4. `<name>-config` is written, its digest and boot hash recorded at
//!    once;
//! 5. TODO(2b): the workload — the migration `Job` of a new version, the
//!    server's `Deployment` (the boot hash on its pod template), the web
//!    UI, the `Service`s, `NetworkPolicy`, `PodDisruptionBudget`,
//!    `Ingress`es, `ServiceMonitor`, the generated keys;
//! 6. TODO(2c): the configuration's confirmation, pod by pod, from
//!    `/healthz` (ADR-0032): `appliedConfigDigest`, each object's
//!    `appliedGeneration`, the refused removals. Until then the instance
//!    stays `ConfigPropagating`.

use api::local_instance::{LocalInstanceSpec, Names};
use api::status::InstanceStatus;
use domain::Failure;
use domain::error::ReasonCode;

use super::{Outcome, api_failure, credential_failure, workload_failure};
use crate::ports::{
    CredentialError, Credentials, ObjectDirectory, OwnedObjects, Owner, StatusLedger,
};
use crate::render::{self, Input, SecretState, SecretStates, paths};

/// What a pass is about.
#[derive(Debug, Clone)]
pub struct InstancePass<'a> {
    pub owner: &'a Owner,
    pub spec: &'a LocalInstanceSpec,
    /// The status as read (the previous pass's).
    pub status: &'a InstanceStatus,
}

/// What a pass needs.
pub struct InstancePorts<'a> {
    pub objects: &'a dyn OwnedObjects,
    pub credentials: &'a dyn Credentials,
    pub ledger: &'a dyn StatusLedger,
    pub directory: &'a dyn ObjectDirectory,
}

/// A pass's result: the `Ready` outcome and the status to publish.
#[derive(Debug, Clone)]
pub struct InstanceReport {
    pub outcome: Outcome,
    pub status: InstanceStatus,
}

/// The status fields a pass writes, for a merge patch against `previous`
/// (`null` clears a field, and an object no longer bound).
#[expect(
    clippy::expect_used,
    reason = "InstanceStatus is a struct of serializable fields: always a JSON object"
)]
pub fn status_fields(
    previous: &InstanceStatus,
    status: &InstanceStatus,
) -> serde_json::Map<String, serde_json::Value> {
    let mut value = serde_json::to_value(status).expect("a status serializes");
    let map = value.as_object_mut().expect("a status is an object");
    map.remove("conditions");
    map.remove("observedGeneration");
    for key in [
        "endpoint",
        "version",
        "migratedVersion",
        "availableReplicas",
        "bootHash",
        "configDigest",
        "appliedConfigDigest",
        "refusedRemovals",
    ] {
        map.entry(key).or_insert(serde_json::Value::Null);
    }
    // A merge patch merges maps: what left `objects` is dropped by name.
    let objects = map
        .entry("objects")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(objects) = objects.as_object_mut() {
        for key in previous.objects.keys() {
            objects
                .entry(key.clone())
                .or_insert(serde_json::Value::Null);
        }
    }
    map.clone()
}

pub async fn reconcile_instance(
    pass: InstancePass<'_>,
    ports: &InstancePorts<'_>,
) -> InstanceReport {
    let mut status = pass.status.clone();
    let ready = run(&pass, ports, &mut status).await;
    InstanceReport {
        outcome: match ready {
            Ok(message) => Outcome::synced(message),
            Err(failure) => Outcome::failed(failure),
        },
        status,
    }
}

async fn run(
    pass: &InstancePass<'_>,
    ports: &InstancePorts<'_>,
    status: &mut InstanceStatus,
) -> Result<String, Failure> {
    let (owner, spec) = (pass.owner, pass.spec);
    let names = Names(&owner.name);
    status.endpoint = Some(format!(
        "http://{}.{}.svc:{}",
        names.server(),
        owner.namespace,
        paths::PORT
    ));

    // 1. The instance's own Secrets.
    for source in render::instance_secrets(&owner.namespace, spec) {
        ports
            .credentials
            .check(&source.namespace, &source.name, &source.key)
            .await
            .map_err(credential_failure)?;
    }

    // 2. Its objects, and theirs.
    let views = ports
        .directory
        .bound_to(owner, &spec.object_selector)
        .await
        .map_err(api_failure)?;
    let mut secrets = SecretStates::new();
    for source in render::object_secrets(&views) {
        let state = match ports
            .credentials
            .check(&source.namespace, &source.name, &source.key)
            .await
        {
            Ok(()) => SecretState::Present,
            Err(CredentialError::Missing(m)) => SecretState::Missing(m),
            Err(CredentialError::NotAllowed(m)) => SecretState::NotAllowed(m),
            Err(e @ CredentialError::Kubernetes(_)) => return Err(credential_failure(e)),
        };
        secrets.insert(source, state);
    }

    // 3. The file.
    let rendered = render::render(&Input {
        namespace: &owner.namespace,
        name: &owner.name,
        spec,
        objects: &views,
        secrets: &secrets,
        previous: &pass.status.objects,
    })
    .map_err(|e| Failure::new(ReasonCode::InvalidSpec, e.to_string()))?;
    status.objects = rendered.objects.clone();

    // 4. Written, and said so at once.
    ports
        .objects
        .apply(owner, &[crate::workload::config_map(owner, &rendered.text)])
        .await
        .map_err(workload_failure)?;
    status.config_digest = Some(rendered.config_digest.clone());
    status.boot_hash = Some(rendered.boot_hash.clone());
    ports
        .ledger
        .record(
            &owner.namespace,
            &owner.name,
            status_fields(pass.status, status),
        )
        .await
        .map_err(api_failure)?;

    // 5. TODO(2b): the workload, from `rendered.env`/`rendered.files`.
    // 6. TODO(2c): the confirmation from `/healthz`.
    let rendered_count = status
        .objects
        .values()
        .filter(|r| r.state == api::status::RenderState::Rendered)
        .count();
    if status.applied_config_digest.is_some()
        && status.applied_config_digest == status.config_digest
    {
        Ok(format!(
            "every pod runs the configuration ({rendered_count} objects rendered)"
        ))
    } else {
        Err(Failure::new(
            ReasonCode::ConfigPropagating,
            format!(
                "configuration written to {} ({rendered_count} objects rendered), waiting for \
                 the pods to load it",
                names.config()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use api::status::RenderState;
    use serde_json::json;
    use weebo_kit_application::test_support::FakeOwnedObjects;

    use super::*;
    use crate::objects::{ObjectSpec, ObjectView};
    use crate::test_support::{FakeCredentials, FakeDirectory, FakeLedger};

    fn spec() -> LocalInstanceSpec {
        serde_json::from_value(json!({
            "database": {"urlSecretRef": {"name": "db", "key": "url"}},
            "attachments": {"s3": {"endpoint": "http://rustfs:9000", "bucket": "notify",
                                   "credentialsSecretRef": {"name": "s3"}}},
            "oidc": {"issuer": "https://id.example", "clientId": "notify"},
        }))
        .unwrap()
    }

    fn owner() -> Owner {
        Owner {
            namespace: "notify".into(),
            name: "main".into(),
            uid: "uid-1".into(),
        }
    }

    fn credentials() -> FakeCredentials {
        let credentials = FakeCredentials::default();
        credentials.insert("notify", "db", &[("url", "postgres://u:p@db/notify")]);
        credentials.insert(
            "notify",
            "s3",
            &[("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "s")],
        );
        credentials
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

    async fn pass(
        status: &InstanceStatus,
        credentials: &FakeCredentials,
        directory: &FakeDirectory,
        objects: &FakeOwnedObjects,
        ledger: &FakeLedger,
    ) -> InstanceReport {
        let (owner, spec) = (owner(), spec());
        reconcile_instance(
            InstancePass {
                owner: &owner,
                spec: &spec,
                status,
            },
            &InstancePorts {
                objects,
                credentials,
                ledger,
                directory,
            },
        )
        .await
    }

    #[tokio::test]
    async fn the_configuration_is_written_and_recorded() {
        let (credentials, objects, ledger) = (
            credentials(),
            FakeOwnedObjects::default(),
            FakeLedger::default(),
        );
        credentials.insert("team", "hook", &[("secret", "s3cr3t")]);
        let directory = FakeDirectory::with(vec![channel("alerts")]);
        let report = pass(
            &InstanceStatus::default(),
            &credentials,
            &directory,
            &objects,
            &ledger,
        )
        .await;
        assert_eq!(report.outcome.reason(), ReasonCode::ConfigPropagating);
        let map = objects.object("ConfigMap", "main-config").unwrap();
        let text = map.manifest["data"]["config.toml"].as_str().unwrap();
        assert!(text.contains("id = \"alerts\""), "{text}");
        assert!(!text.contains("s3cr3t"));
        let status = &report.status;
        assert_eq!(
            status.config_digest.as_deref(),
            Some(weebo_kit_application::workload::content_hash(text.as_bytes()).as_str())
        );
        assert!(status.boot_hash.is_some());
        assert_eq!(
            status.objects["NotifyChannel/team/alerts"].state,
            RenderState::Rendered
        );
        assert_eq!(
            status.endpoint.as_deref(),
            Some("http://main.notify.svc:8080")
        );
        // Recorded right after the write.
        let recorded = ledger.last().unwrap();
        assert_eq!(recorded["configDigest"], json!(status.config_digest));

        // Confirmed (2c will set it from /healthz): Ready.
        let mut confirmed = report.status.clone();
        confirmed.applied_config_digest = confirmed.config_digest.clone();
        let again = pass(&confirmed, &credentials, &directory, &objects, &ledger).await;
        assert_eq!(again.outcome.reason(), ReasonCode::Reconciled);
    }

    #[tokio::test]
    async fn a_secret_missing_stops_the_instance_or_leaves_its_object_out() {
        let (objects, ledger) = (FakeOwnedObjects::default(), FakeLedger::default());
        let directory = FakeDirectory::with(vec![channel("alerts")]);
        let none = FakeCredentials::default();
        let report = pass(
            &InstanceStatus::default(),
            &none,
            &directory,
            &objects,
            &ledger,
        )
        .await;
        assert_eq!(report.outcome.reason(), ReasonCode::SecretMissing);
        assert!(objects.object("ConfigMap", "main-config").is_none());

        let credentials = credentials();
        credentials.insert_unlabelled("team", "hook", &[("secret", "s3cr3t")]);
        let report = pass(
            &InstanceStatus::default(),
            &credentials,
            &directory,
            &objects,
            &ledger,
        )
        .await;
        assert_eq!(report.outcome.reason(), ReasonCode::ConfigPropagating);
        assert_eq!(
            report.status.objects["NotifyChannel/team/alerts"].state,
            RenderState::SecretNotAllowed
        );
    }

    #[tokio::test]
    async fn an_object_gone_is_dropped_from_the_status_by_name() {
        let (credentials, objects, ledger) = (
            credentials(),
            FakeOwnedObjects::default(),
            FakeLedger::default(),
        );
        credentials.insert("team", "hook", &[("secret", "s3cr3t")]);
        let first = pass(
            &InstanceStatus::default(),
            &credentials,
            &FakeDirectory::with(vec![channel("alerts")]),
            &objects,
            &ledger,
        )
        .await;
        let second = pass(
            &first.status,
            &credentials,
            &FakeDirectory::default(),
            &objects,
            &ledger,
        )
        .await;
        assert!(second.status.objects.is_empty());
        let fields = status_fields(&first.status, &second.status);
        assert_eq!(
            fields["objects"],
            json!({"NotifyChannel/team/alerts": null})
        );
    }

    #[tokio::test]
    async fn an_owned_object_taken_is_a_conflict() {
        let (credentials, objects, ledger) = (
            credentials(),
            FakeOwnedObjects::default(),
            FakeLedger::default(),
        );
        objects.foreign("ConfigMap", "main-config");
        let report = pass(
            &InstanceStatus::default(),
            &credentials,
            &FakeDirectory::default(),
            &objects,
            &ledger,
        )
        .await;
        assert_eq!(report.outcome.reason(), ReasonCode::OwnedObjectConflict);
    }
}
