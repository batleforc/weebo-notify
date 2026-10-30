use std::collections::BTreeMap;

use api::api_key::{ApiKeySpec, ApiKeyStatus};
use api::common::Scope;
use api::status::InstanceStatus;
use k8s_openapi::api::core::v1::Secret;
use weebo_kit_api::{ObjectRef, SecretTarget};
use weebo_kit_application::workload::{OwnedObject, OwnedObjects};

use super::*;
use crate::ports::AccountView;
use crate::test_support::{
    FakeAccounts, FakeInstances, FakeIssuer, FakeOwnedObjects, FakeSecretStore,
};

struct World {
    instances: FakeInstances,
    accounts: FakeAccounts,
    objects: FakeOwnedObjects,
    issuer: FakeIssuer,
    secrets: FakeSecretStore,
}

impl World {
    /// `main` of `team` runs with its key issuer; `ci` is applied on it.
    async fn new() -> Self {
        let world = Self {
            instances: FakeInstances::with_status(InstanceStatus {
                endpoint: Some("http://main.team.svc:8080".into()),
                ..Default::default()
            }),
            accounts: FakeAccounts::default(),
            objects: FakeOwnedObjects::default(),
            issuer: FakeIssuer::default(),
            secrets: FakeSecretStore::default(),
        };
        world.account(true);
        let mut secret = Secret::default();
        secret.metadata.name = Some("main-key-issuer".into());
        secret.data = Some(BTreeMap::from([(
            "key".to_string(),
            k8s_openapi::ByteString(b"issuer-secret".to_vec()),
        )]));
        world
            .objects
            .apply(&owner(), &[OwnedObject::of(&secret, true)])
            .await
            .unwrap();
        world
    }

    fn account(&self, ready: bool) {
        self.accounts.insert(
            "team",
            "ci",
            AccountView {
                id: Some("ci-bot".into()),
                instance: Some(BoundInstance::local("team", "main")),
                ready,
            },
        );
    }

    fn ports(&self) -> ApiKeyPorts<'_> {
        ApiKeyPorts {
            instances: &self.instances,
            accounts: &self.accounts,
            objects: &self.objects,
            issuer: &self.issuer,
            secrets: &self.secrets,
        }
    }

    async fn pass(
        &self,
        spec: &ApiKeySpec,
        status: &ApiKeyStatus,
        rotate: Option<&str>,
    ) -> ApiKeyReport {
        reconcile_api_key(
            ApiKeyPass {
                namespace: "team",
                name: "deploy",
                spec,
                status,
                rotate,
            },
            self.ports(),
        )
        .await
    }

    fn target(&self) -> Option<BTreeMap<String, String>> {
        self.secrets.get("team/deploy")
    }
}

fn owner() -> Owner {
    Owner {
        namespace: "team".into(),
        name: "main".into(),
        uid: String::new(),
    }
}

fn spec() -> ApiKeySpec {
    ApiKeySpec {
        instance_ref: None,
        service_account_ref: ObjectRef { name: "ci".into() },
        scopes: vec![Scope::Publish],
        channels: vec!["alerts".into()],
        secret_targets: Vec::new(),
    }
}

/// The status a reconcile writes from its report.
fn status_of(report: &ApiKeyReport) -> ApiKeyStatus {
    ApiKeyStatus {
        instance: report.outcome.instance.clone(),
        service_account: report.service_account.clone(),
        key_id: report.key_id.clone(),
        key_prefix: report.key_prefix.clone(),
        last_rotation: report.last_rotation.clone(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_key_is_minted_written_then_kept() {
    let world = World::new().await;
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    assert_eq!(
        report.outcome.reason(),
        ReasonCode::Reconciled,
        "{:?}",
        report.outcome
    );
    assert_eq!(report.key_id.as_deref(), Some("key-1"));
    assert_eq!(report.service_account.as_deref(), Some("ci-bot"));
    assert!(report.events.is_empty());
    let target = world.target().unwrap();
    assert_eq!(target["NOTIFY_API_KEY"], "wnk_p1_secret");
    assert_eq!(target["NOTIFY_URL"], "http://main.team.svc:8080");
    // Minted as the operator's key issuer, labelled for this CR.
    let tokens = world.issuer.tokens.lock().unwrap().clone();
    assert!(
        tokens
            .iter()
            .all(|t| t == "wki_weebo-notify-operator_issuer-secret")
    );
    assert_eq!(
        world.issuer.keys()[0].label.as_deref(),
        Some("weebo-notify:team/deploy")
    );

    // Nothing changed: the same key.
    let again = world.pass(&spec(), &status_of(&report), None).await;
    assert_eq!(again.outcome.reason(), ReasonCode::Reconciled);
    assert_eq!(again.key_id.as_deref(), Some("key-1"));
    assert_eq!(world.issuer.keys().len(), 1);
}

#[tokio::test]
async fn a_lost_target_a_rotation_or_new_scopes_mint_a_new_key_and_revoke_the_old() {
    let world = World::new().await;
    let first = world.pass(&spec(), &ApiKeyStatus::default(), None).await;

    // The Secret was deleted: its value can't be read back.
    world.secrets.remove("team/deploy");
    let lost = world.pass(&spec(), &status_of(&first), None).await;
    assert_eq!(lost.key_id.as_deref(), Some("key-2"));
    assert_eq!(lost.events[0].0, ReasonCode::KeyRotated);
    assert_eq!(world.target().unwrap()["NOTIFY_API_KEY"], "wnk_p2_secret");
    let live: Vec<_> = world.issuer.live().into_iter().map(|k| k.id).collect();
    assert_eq!(live, ["key-2"]);

    // Asked through the annotation: once per value.
    let rotated = world.pass(&spec(), &status_of(&lost), Some("1")).await;
    assert_eq!(rotated.key_id.as_deref(), Some("key-3"));
    assert_eq!(rotated.last_rotation.as_deref(), Some("1"));
    let same = world.pass(&spec(), &status_of(&rotated), Some("1")).await;
    assert_eq!(same.key_id.as_deref(), Some("key-3"));

    // Other scopes.
    let mut wider = spec();
    wider.scopes.push(Scope::Subscribe);
    let changed = world.pass(&wider, &status_of(&same), Some("1")).await;
    assert_eq!(changed.key_id.as_deref(), Some("key-4"));
    let live: Vec<_> = world.issuer.live().into_iter().map(|k| k.id).collect();
    assert_eq!(live, ["key-4"]);
}

#[tokio::test]
async fn a_key_left_by_a_failed_pass_is_revoked_and_others_are_left_alone() {
    let world = World::new().await;
    let mut stray = crate::ports::IssuedKey {
        id: "stray".into(),
        prefix: "s".into(),
        label: Some("weebo-notify:team/deploy".into()),
        service_account: Some("ci-bot".into()),
        scopes: vec![Scope::Publish],
        channels: vec!["alerts".into()],
        revoked: false,
    };
    world.issuer.insert(stray.clone());
    stray.id = "someone-else".into();
    stray.label = Some("CI, by hand".into());
    world.issuer.insert(stray);
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::Reconciled);
    let live: Vec<_> = world.issuer.live().into_iter().map(|k| k.id).collect();
    assert_eq!(live, ["someone-else", "key-1"]);
}

#[tokio::test]
async fn a_key_that_cannot_be_written_is_revoked_at_once() {
    let world = World::new().await;
    world.secrets.fail_writes(true);
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::SecretTargetFailed);
    assert_eq!(report.key_id, None);
    assert!(world.issuer.live().is_empty());
}

#[tokio::test]
async fn targets_stay_in_the_namespace() {
    let world = World::new().await;
    let mut elsewhere = spec();
    elsewhere.secret_targets = vec![SecretTarget {
        backend: Default::default(),
        name: "creds".into(),
        namespace: Some("other".into()),
        path: None,
    }];
    let report = world.pass(&elsewhere, &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::SecretTargetFailed);
    assert!(world.issuer.keys().is_empty());
}

#[tokio::test]
async fn it_waits_for_its_account_and_the_key_issuer() {
    let world = World::new().await;
    world.account(false);
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::ServiceAccountNotReady);
    assert!(world.issuer.keys().is_empty());

    world.account(true);
    world
        .issuer
        .fail(Some(NotifyApiError::Unauthorized("401".into())));
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::KeyIssuerUnavailable);

    world
        .issuer
        .fail(Some(NotifyApiError::Refused("403 scope".into())));
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::KeyRefused);

    // An account of another instance.
    world.issuer.fail(None);
    world.accounts.insert(
        "team",
        "ci",
        AccountView {
            id: Some("ci-bot".into()),
            instance: Some(BoundInstance::local("team", "other")),
            ready: true,
        },
    );
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::ServiceAccountNotReady);
    let missing = World::new().await;
    let mut other = spec();
    other.service_account_ref.name = "nobody".into();
    let report = missing.pass(&other, &ApiKeyStatus::default(), None).await;
    assert_eq!(report.outcome.reason(), ReasonCode::ServiceAccountNotReady);
}

#[tokio::test]
async fn a_deleted_key_is_revoked_and_its_targets_deleted() {
    let world = World::new().await;
    let report = world.pass(&spec(), &ApiKeyStatus::default(), None).await;
    let bound = report.outcome.instance.clone().unwrap();
    let release = |orphaned| ApiKeyRelease {
        namespace: "team",
        name: "deploy",
        spec: &SPEC,
        recorded: Some(&bound),
        orphaned,
    };
    static SPEC: std::sync::LazyLock<ApiKeySpec> = std::sync::LazyLock::new(spec);
    // Force-orphaned: left as is.
    release_api_key(release(true), world.ports()).await.unwrap();
    assert_eq!(world.issuer.live().len(), 1);
    assert!(world.target().is_some());

    release_api_key(release(false), world.ports())
        .await
        .unwrap();
    assert!(world.issuer.live().is_empty());
    assert!(world.target().is_none());

    // Its instance gone: the targets only.
    let gone = World {
        instances: FakeInstances::default(),
        ..World::new().await
    };
    gone.secrets
        .insert_foreign("team/deploy", "NotifyApiKey/team/deploy", BTreeMap::new());
    release_api_key(release(false), gone.ports()).await.unwrap();
    assert!(gone.target().is_none());
}
