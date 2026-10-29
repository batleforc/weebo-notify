//! Integration test: the CRDs installed on a real API server (envtest), and
//! their schema and CEL rules refusing what they say they refuse — no
//! operator running, no webhook registered: the API server alone.

#![expect(clippy::panic, reason = "test helpers fail the test on purpose")]

use api::{Channel, LocalInstance};
use kube::api::{Api, Patch, PatchParams, PostParams};
use serde_json::{Value, json};

#[expect(dead_code, reason = "the suites share the helpers, each uses some")]
mod common;

use common::{cr, instance_spec};

/// The API server's refusal of `object`, `None` when it was created.
async fn refused<K>(api: &Api<K>, object: &K) -> Option<String>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
{
    match api.create(&PostParams::default(), object).await {
        Ok(_) => None,
        Err(kube::Error::Api(e)) => {
            assert_eq!(e.code, 422, "{e:?}");
            Some(e.message)
        }
        Err(e) => panic!("{e}"),
    }
}

fn instance(name: &str, extra: Value) -> LocalInstance {
    cr("NotifyLocalInstance", name, "team", instance_spec(extra))
}

fn channel(name: &str, spec: Value) -> Channel {
    cr("NotifyChannel", name, "team", spec)
}

#[tokio::test]
async fn the_crds_refuse_what_their_rules_say() {
    let cluster = testkit::envtest::start().await;
    let client = cluster.client();
    common::namespace(&client, "team", &[]).await;
    let instances: Api<LocalInstance> = Api::namespaced(client.clone(), "team");
    let channels: Api<Channel> = Api::namespaced(client.clone(), "team");

    // The instance.
    assert_eq!(
        refused(&instances, &instance("main", json!({}))).await,
        None
    );
    let name = refused(&instances, &instance("1notify", json!({})))
        .await
        .unwrap();
    assert!(name.contains("DNS-1035"), "{name}");
    let long = "n".repeat(51);
    assert!(
        refused(&instances, &instance(&long, json!({})))
            .await
            .is_some()
    );
    let filesystem = json!({"filesystem": {"publicBaseUrl": "https://notify.example.com"}});
    let replicas = refused(
        &instances,
        &instance("fs", json!({"replicas": 2, "attachments": filesystem})),
    )
    .await
    .unwrap();
    assert!(replicas.contains("S3 attachments"), "{replicas}");
    assert_eq!(
        refused(
            &instances,
            &instance("fs1", json!({"replicas": 1, "attachments": filesystem}))
        )
        .await,
        None
    );
    let both = refused(
        &instances,
        &instance(
            "both",
            json!({"attachments": {"filesystem": {"publicBaseUrl": "https://n.example.com"},
                   "s3": {"endpoint": "http://s3", "bucket": "b", "credentialsSecretRef": {"name": "s3"}}}}),
        ),
    )
    .await
    .unwrap();
    assert!(both.contains("exactly one of s3 and filesystem"), "{both}");
    assert!(
        refused(&instances, &instance("none", json!({"attachments": {}})))
            .await
            .is_some()
    );
    let public = refused(
        &instances,
        &instance(
            "path",
            json!({"server": {"publicUrl": "https://notify.example.com/api"}}),
        ),
    )
    .await
    .unwrap();
    assert!(public.contains("publicUrl"), "{public}");
    for url in [
        "https://notify.example.com",
        "http://notify.example.com:8080/",
    ] {
        let name = format!("url{}", url.len());
        assert_eq!(
            refused(
                &instances,
                &instance(&name, json!({"server": {"publicUrl": url}}))
            )
            .await,
            None,
            "{url}"
        );
    }

    // A channel.
    let access = refused(
        &channels,
        &channel(
            "two",
            json!({"access": [{"oidcGroup": "ops", "email": "a@example.com", "scopes": ["publish"]}]}),
        ),
    )
    .await
    .unwrap();
    assert!(access.contains("exactly one of"), "{access}");
    let webhook = refused(
        &channels,
        &channel(
            "hook",
            json!({"sinks": [{"type": "webhook", "url": "https://hooks.example.com"}]}),
        ),
    )
    .await
    .unwrap();
    assert!(webhook.contains("secretRef"), "{webhook}");
    let feeds = refused(
        &channels,
        &channel(
            "feeds",
            json!({"feeds": [
                {"url": "https://blog.example.com/feed.xml", "publisher": "rss"},
                {"url": "https://blog.example.com/feed.xml", "publisher": "rss"},
            ]}),
        ),
    )
    .await
    .unwrap();
    assert!(feeds.contains("once per channel"), "{feeds}");
    // The id: immutable once set.
    channels
        .create(
            &PostParams::default(),
            &channel("alerts", json!({"id": "alerts"})),
        )
        .await
        .unwrap();
    let changed = channels
        .patch(
            "alerts",
            &PatchParams::default(),
            &Patch::Merge(json!({"spec": {"id": "pages"}})),
        )
        .await
        .unwrap_err();
    assert!(changed.to_string().contains("id is immutable"), "{changed}");
    let bad_id = refused(&channels, &channel("upper", json!({"id": "Alerts"}))).await;
    assert!(bad_id.is_some());
}

/// The admission decision with the real adapters reading this API server
/// (envtest can't call a webhook back without a TLS listener and a
/// `ValidatingWebhookConfiguration`: the router is driven in-process).
#[tokio::test]
async fn the_admission_reads_the_cluster_and_answers_with_notify_s_words() {
    use std::sync::Arc;

    use adapters_inbound::admission::{NotifyAdmission, handle};
    use adapters_outbound::{KubeCredentials, KubeObjects, LocalInstanceDirectory};
    use kube::core::DynamicObject;
    use kube::core::admission::AdmissionReview;

    let cluster = testkit::envtest::start().await;
    let client = cluster.client();
    common::namespace(&client, "team", &[]).await;
    let admission = NotifyAdmission {
        credentials: Arc::new(KubeCredentials::new(client.clone())),
        attached: Arc::new(LocalInstanceDirectory::new(client.clone())),
        directory: Arc::new(KubeObjects::new(client.clone())),
        allowed_images: common::allowed_images(),
        default_image: format!("{}:{}", api::notify::SERVER_IMAGE, api::notify::VERSION),
        default_web_image: format!("{}:{}", api::notify::WEB_IMAGE, api::notify::VERSION),
    };
    let review = |kind: &str, object: Value| -> AdmissionReview<DynamicObject> {
        serde_json::from_value(json!({
            "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
            "request": {
                "uid": "u", "operation": "CREATE", "namespace": "team", "name": "x",
                "kind": {"group": "notify.weebo.io", "version": "v1alpha1", "kind": kind},
                "resource": {"group": "notify.weebo.io", "version": "v1alpha1", "resource": "x"},
                "userInfo": {}, "object": object, "dryRun": false,
            }
        }))
        .unwrap()
    };
    let response =
        |r: AdmissionReview<DynamicObject>| serde_json::to_value(r.response.unwrap()).unwrap();

    // No Secret yet: admitted, with a warning.
    let instance = json!({"apiVersion": "notify.weebo.io/v1alpha1", "kind": "NotifyLocalInstance",
        "metadata": {"name": "main", "namespace": "team"}, "spec": instance_spec(json!({}))});
    let answer =
        response(handle(&admission, review("NotifyLocalInstance", instance.clone())).await);
    assert_eq!(answer["allowed"], true);
    assert!(answer["warnings"].to_string().contains("db"), "{answer}");
    Api::<LocalInstance>::namespaced(client.clone(), "team")
        .create(
            &PostParams::default(),
            &serde_json::from_value(instance).unwrap(),
        )
        .await
        .unwrap();

    // A transform whose CEL doesn't compile: Notify's compiler says why.
    let transform = json!({"apiVersion": "notify.weebo.io/v1alpha1", "kind": "NotifyTransform",
        "metadata": {"name": "grafana", "namespace": "team"},
        "spec": {"channels": ["alerts"], "title": "body.("}});
    let answer = response(handle(&admission, review("NotifyTransform", transform)).await);
    assert_eq!(answer["allowed"], false);
    assert_eq!(answer["status"]["reason"], "InvalidSpec");
    let message = answer["status"]["message"].as_str().unwrap();
    assert!(message.contains("grafana"), "{message}");
    // A channel naming nothing missing: admitted.
    let channel = json!({"apiVersion": "notify.weebo.io/v1alpha1", "kind": "NotifyChannel",
        "metadata": {"name": "alerts", "namespace": "team"}, "spec": {}});
    assert_eq!(
        response(handle(&admission, review("NotifyChannel", channel)).await)["allowed"],
        true
    );
}
