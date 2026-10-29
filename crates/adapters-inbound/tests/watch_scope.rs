//! Integration test: the controllers limited to one namespace and a label
//! selector (`WatchScope`, the chart's `watch.*`): what is outside is
//! never touched — no finalizer, no status — and the watches never fail.
//! (On the admin client: the RBAC the chart renders for a scope is the
//! library chart's, tested there.)

use std::collections::BTreeMap;
use std::time::Duration;

use adapters_inbound::controller;
use api::{Channel, LocalInstance};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::Resource;
use kube::api::{Api, PostParams};
use serde_json::json;
use testkit::polling::wait_for;
use weebo_kit_runtime::WatchScope;

#[expect(dead_code, reason = "the suites share the helpers, each uses some")]
mod common;

use common::{POLL, TIMEOUT, cr, instance_spec};

fn labelled<K: Resource>(mut object: K, shard: &str) -> K {
    object.meta_mut().labels = Some(BTreeMap::from([("shard".into(), shard.into())]));
    object
}

#[tokio::test]
async fn a_scoped_operator_sees_only_its_objects() {
    let cluster = testkit::envtest::start().await;
    let client = cluster.client();
    for ns in ["team-a", "elsewhere"] {
        common::namespace(&client, ns, &[]).await;
        common::instance_secrets(&client, ns).await;
    }
    let scope = WatchScope::namespace("team-a")
        .with_labels(LabelSelector {
            match_labels: Some(BTreeMap::from([("shard".into(), "a".into())])),
            ..Default::default()
        })
        .unwrap();
    let (ctx, health, _) = common::ctx(&client, scope);
    tokio::spawn(controller::local_instance::run(client.clone(), ctx.clone()));
    tokio::spawn(controller::object::run::<Channel>(client.clone(), ctx));
    let pp = PostParams::default();
    for ns in ["team-a", "elsewhere"] {
        let instance: LocalInstance =
            cr("NotifyLocalInstance", "main", ns, instance_spec(json!({})));
        Api::<LocalInstance>::namespaced(client.clone(), ns)
            .create(&pp, &labelled(instance, "a"))
            .await
            .unwrap();
    }
    let team: Api<Channel> = Api::namespaced(client.clone(), "team-a");
    let elsewhere: Api<Channel> = Api::namespaced(client.clone(), "elsewhere");
    let channel = |name: &str, ns: &str| -> Channel { cr("NotifyChannel", name, ns, json!({})) };
    team.create(&pp, &labelled(channel("in-scope", "team-a"), "a"))
        .await
        .unwrap();
    team.create(&pp, &labelled(channel("other-shard", "team-a"), "b"))
        .await
        .unwrap();
    elsewhere
        .create(&pp, &labelled(channel("other-namespace", "elsewhere"), "a"))
        .await
        .unwrap();

    // In scope: bound to its namespace's instance, rendered into its file.
    let bound = wait_for(&team, "in-scope", TIMEOUT, POLL, |c| {
        c.status.as_ref()?.instance.clone()
    })
    .await;
    assert_eq!(bound.name, "main");
    let instances: Api<LocalInstance> = Api::namespaced(client.clone(), "team-a");
    wait_for(&instances, "main", TIMEOUT, POLL, |i| {
        i.status
            .as_ref()?
            .objects
            .contains_key("NotifyChannel/team-a/in-scope")
            .then_some(())
    })
    .await;

    // Out of scope: never touched, and the watches never failed.
    tokio::time::sleep(Duration::from_secs(2)).await;
    for (api, name) in [(&team, "other-shard"), (&elsewhere, "other-namespace")] {
        let untouched = api.get(name).await.unwrap();
        assert!(untouched.status.is_none(), "{name} was reconciled");
        assert!(
            untouched.metadata.finalizers.is_none(),
            "{name} got the finalizer"
        );
    }
    let outside = Api::<LocalInstance>::namespaced(client.clone(), "elsewhere")
        .get("main")
        .await
        .unwrap();
    assert!(
        outside.status.is_none(),
        "the instance outside was reconciled"
    );
    let rendered = instances.get("main").await.unwrap().status.unwrap();
    assert!(
        !rendered
            .objects
            .contains_key("NotifyChannel/team-a/other-shard")
    );
    assert_eq!(health.live(), Ok(()));
}
