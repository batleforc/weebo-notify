use serde_json::{Value, json};

use super::*;
use crate::test_support::{FakeCredentials, FakeDirectory, FakeInstances};

const IMAGE: &str = "ghcr.io/batleforc/weebo-notify:0.3.0";
const WEB: &str = "ghcr.io/batleforc/weebo-notify-web:0.3.0";

fn allowed() -> Vec<String> {
    vec![
        api::notify::SERVER_IMAGE.to_string(),
        api::notify::WEB_IMAGE.to_string(),
    ]
}

fn instance(extra: Value) -> LocalInstance {
    let mut spec = json!({
        "database": {"urlSecretRef": {"name": "db", "key": "url"}},
        "attachments": {"s3": {"endpoint": "http://rustfs:9000", "bucket": "notify",
                               "credentialsSecretRef": {"name": "s3"}}},
        "oidc": {"issuer": "https://id.example", "clientId": "notify"},
    });
    if let (Some(spec), Value::Object(extra)) = (spec.as_object_mut(), extra) {
        spec.extend(extra);
    }
    LocalInstance::new("main", serde_json::from_value(spec).unwrap())
}

fn labelled() -> FakeCredentials {
    let credentials = FakeCredentials::default();
    credentials.insert("team", "db", &[("url", "postgres://u:p@db/notify")]);
    credentials.insert(
        "team",
        "s3",
        &[("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "s")],
    );
    credentials.insert("team", "hook", &[("secret", "s3cr3t")]);
    credentials
}

async fn admit_instance(
    instance: &LocalInstance,
    credentials: &FakeCredentials,
) -> Result<Warnings, Failure> {
    let allowed = allowed();
    check_instance(
        "main",
        "team",
        instance,
        &Settings {
            allowed_images: &allowed,
            default_image: IMAGE,
            default_web_image: WEB,
        },
        credentials,
    )
    .await
}

#[tokio::test]
async fn an_instance_notify_accepts_is_admitted() {
    let warnings = admit_instance(&instance(json!({})), &labelled())
        .await
        .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[tokio::test]
async fn what_notify_refuses_is_refused_with_its_own_words() {
    let refused = admit_instance(
        &instance(
            json!({"database": {"urlSecretRef": {"name": "db", "key": "url"},
                                       "maxConnections": 5}}),
        ),
        &labelled(),
    )
    .await
    .unwrap_err();
    assert_eq!(refused.reason, ReasonCode::InvalidSpec);
    // notify-config's message, as the server would log it.
    assert!(
        refused.message.contains("[database] max_connections = 5"),
        "{}",
        refused.message
    );
    let audit = admit_instance(
        &instance(json!({"retention": {"audit": "30d"}})),
        &labelled(),
    )
    .await
    .unwrap_err();
    assert!(audit.message.contains("90-day floor"), "{}", audit.message);
}

#[tokio::test]
async fn extra_config_may_not_hold_the_crs_sections_secrets_or_typed_keys() {
    for (extra, said) in [
        (
            json!({"channels": {"managed": [{"id": "x"}]}}),
            "comes from the CRs",
        ),
        (json!({"transforms": []}), "comes from the CRs"),
        (
            json!({"sinks": {"fingerprint_key": "k"}}),
            "comes from the CRs",
        ),
        (
            json!({"oidc": {"issuer": "https://other"}}),
            "the instance's spec renders",
        ),
    ] {
        let refused = admit_instance(&instance(json!({"extraConfig": extra})), &labelled())
            .await
            .unwrap_err();
        assert_eq!(refused.reason, ReasonCode::InvalidSpec);
        assert!(
            refused.message.contains(said),
            "{extra}: {}",
            refused.message
        );
    }
    assert!(
        admit_instance(
            &instance(json!({"extraConfig": {"devices": {"max_per_user": 5}}})),
            &labelled()
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn an_image_off_the_allow_list_is_refused() {
    for image in [
        json!({"image": {"repository": "evil/miner"}}),
        json!({"web": {"image": {"repository": "evil/nginx"}}}),
    ] {
        let refused = admit_instance(&instance(image.clone()), &labelled())
            .await
            .unwrap_err();
        assert_eq!(refused.reason, ReasonCode::InvalidSpec, "{image}");
        assert!(
            refused.message.contains("allow-list"),
            "{}",
            refused.message
        );
    }
}

#[tokio::test]
async fn the_version_guard_holds_at_admission_unless_approved() {
    let mut unsupported = instance(json!({"image": {"tag": "9.9.9"}}));
    let refused = admit_instance(&unsupported, &labelled()).await.unwrap_err();
    assert_eq!(refused.reason, ReasonCode::UnsupportedVersion);
    unsupported.metadata.annotations = Some(BTreeMap::from([(
        api::annotations::APPROVED_UPGRADE.to_string(),
        "9.9.9".to_string(),
    )]));
    assert!(admit_instance(&unsupported, &labelled()).await.is_ok());
    // A downgrade below the migrated schema, on an update.
    let mut downgrade = instance(json!({}));
    downgrade.status = Some(api::status::InstanceStatus {
        migrated_version: Some("0.4.0".into()),
        ..Default::default()
    });
    assert_eq!(
        admit_instance(&downgrade, &labelled())
            .await
            .unwrap_err()
            .reason,
        ReasonCode::UnsupportedVersion
    );
}

#[tokio::test]
async fn a_secret_missing_is_a_warning_one_not_labelled_a_refusal() {
    let missing = admit_instance(&instance(json!({})), &FakeCredentials::default())
        .await
        .unwrap();
    assert_eq!(missing.len(), 3, "{missing:?}");
    assert!(missing[0].contains("db"), "{missing:?}");
    let unlabelled = labelled();
    unlabelled.insert_unlabelled("team", "db", &[("url", "x")]);
    assert_eq!(
        admit_instance(&instance(json!({})), &unlabelled)
            .await
            .unwrap_err()
            .reason,
        ReasonCode::SecretNotAllowed
    );
}

fn view(kind: &str, name: &str, created: &str, spec: Value) -> ObjectView {
    let spec = match kind {
        "sa" => ObjectSpec::ServiceAccount(serde_json::from_value(spec).unwrap()),
        "ch" => ObjectSpec::Channel(serde_json::from_value(spec).unwrap()),
        "tr" => ObjectSpec::Transform(serde_json::from_value(spec).unwrap()),
        _ => ObjectSpec::TrustPolicy(serde_json::from_value(spec).unwrap()),
    };
    ObjectView {
        namespace: "team".into(),
        name: name.into(),
        created: Some(created.parse().unwrap()),
        generation: Some(1),
        spec,
        deleting: false,
    }
}

async fn admit_object(
    view: ObjectView,
    instances: &FakeInstances,
    bound: Vec<ObjectView>,
    credentials: &FakeCredentials,
) -> Result<Warnings, Failure> {
    check_object(
        ObjectReview {
            view,
            instance_ref: None,
            recorded: None,
        },
        &ObjectPorts {
            attached: instances,
            directory: &FakeDirectory::with(bound),
            credentials,
        },
    )
    .await
}

fn served() -> FakeInstances {
    FakeInstances::with_status(Default::default())
}

#[tokio::test]
async fn a_transform_whose_cel_doesn_t_compile_is_refused_with_notify_s_error() {
    let broken = view(
        "tr",
        "grafana",
        "2026-01-01T00:00:00Z",
        json!({"channels": ["alerts"], "title": "body.("}),
    );
    let refused = admit_object(broken, &served(), Vec::new(), &labelled())
        .await
        .unwrap_err();
    assert_eq!(refused.reason, ReasonCode::InvalidSpec);
    assert!(refused.message.contains("grafana"), "{}", refused.message);
    // The same without an instance: notify's compiler still says no.
    let broken = view(
        "tr",
        "grafana",
        "2026-01-01T00:00:00Z",
        json!({"channels": ["alerts"], "title": "body.("}),
    );
    assert!(
        admit_object(broken, &FakeInstances::default(), Vec::new(), &labelled())
            .await
            .is_err()
    );
    let fine = view(
        "tr",
        "grafana",
        "2026-01-01T00:00:00Z",
        json!({"channels": ["alerts"], "title": "body.title"}),
    );
    let warnings = admit_object(fine, &served(), Vec::new(), &labelled())
        .await
        .unwrap();
    // `alerts` is no NotifyChannel of the instance.
    assert!(
        warnings.iter().any(|w| w.contains("alerts")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn no_instance_yet_an_id_held_or_a_secret_missing_are_warnings() {
    let account = || view("sa", "bot", "2026-02-01T00:00:00Z", json!({"id": "ci"}));
    let alone = admit_object(
        account(),
        &FakeInstances::default(),
        Vec::new(),
        &labelled(),
    )
    .await
    .unwrap();
    assert!(alone[0].contains("no instance serves it yet"), "{alone:?}");
    let older = view("sa", "ci", "2026-01-01T00:00:00Z", json!({}));
    let held = admit_object(account(), &served(), vec![older], &labelled())
        .await
        .unwrap();
    assert!(held.iter().any(|w| w.contains("team/ci")), "{held:?}");
    let hook = view(
        "ch",
        "alerts",
        "2026-01-01T00:00:00Z",
        json!({"sinks": [{"type": "webhook", "url": "https://hooks.example.com",
                          "secretRef": {"name": "hook", "key": "secret"}}]}),
    );
    let missing = admit_object(hook, &served(), Vec::new(), &FakeCredentials::default())
        .await
        .unwrap();
    assert!(missing.iter().any(|w| w.contains("hook")), "{missing:?}");
}

#[tokio::test]
async fn a_channel_is_checked_against_its_instance_and_its_accounts() {
    let feed = |grant: bool| {
        let access = if grant {
            json!([{"serviceAccount": "rss", "scopes": ["publish"]}])
        } else {
            json!([])
        };
        view(
            "ch",
            "news",
            "2026-01-02T00:00:00Z",
            json!({"access": access,
                   "feeds": [{"url": "https://blog.example.com/feed.xml", "publisher": "rss"}]}),
        )
    };
    let rss = view("sa", "rss", "2026-01-01T00:00:00Z", json!({}));
    assert!(
        admit_object(feed(true), &served(), vec![rss.clone()], &labelled())
            .await
            .is_ok()
    );
    let refused = admit_object(feed(false), &served(), vec![rss], &labelled())
        .await
        .unwrap_err();
    assert!(refused.message.contains("publish"), "{}", refused.message);
    // No instance: the feed's publisher stands in, its grant still checked.
    assert!(
        admit_object(
            feed(true),
            &FakeInstances::default(),
            Vec::new(),
            &labelled()
        )
        .await
        .is_ok()
    );
    assert!(
        admit_object(
            feed(false),
            &FakeInstances::default(),
            Vec::new(),
            &labelled()
        )
        .await
        .is_err()
    );
    // An email sink on an instance without an SMTP relay.
    let mail = view(
        "ch",
        "mail",
        "2026-01-01T00:00:00Z",
        json!({"sinks": [{"type": "email", "address": "ops@example.com"}]}),
    );
    let refused = admit_object(mail, &served(), Vec::new(), &labelled())
        .await
        .unwrap_err();
    assert!(refused.message.contains("smtp"), "{}", refused.message);
}
