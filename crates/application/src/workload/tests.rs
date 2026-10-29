use serde_json::{Value, json};

use super::*;
use crate::render::{self, Input, SecretStates};

fn spec(extra: Value) -> LocalInstanceSpec {
    let mut base = json!({
        "database": {"urlSecretRef": {"name": "db", "key": "url"}},
        "attachments": {"s3": {"endpoint": "http://rustfs.storage:9000", "bucket": "notify",
                               "credentialsSecretRef": {"name": "s3"}}},
        "oidc": {"issuer": "https://id.example.com/application/o/notify/", "clientId": "notify"},
        "webPush": {"vapidKeySecretRef": {"name": "vapid", "key": "key.pem"},
                    "contact": "mailto:ops@example.com"},
        "ingress": {"className": "nginx",
                    "api": {"host": "notify.example.com", "tlsSecretName": "notify-tls"},
                    "web": {"host": "app.example.com"}},
    });
    if let (Some(base), Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
    serde_json::from_value(base).unwrap()
}

fn rendered(spec: &LocalInstanceSpec) -> Rendered {
    render::render(&Input {
        namespace: "team",
        name: "main",
        spec,
        objects: &[],
        secrets: &SecretStates::new(),
        previous: &BTreeMap::new(),
    })
    .unwrap()
}

struct Fixture {
    spec: LocalInstanceSpec,
    rendered: Rendered,
    generated: BTreeMap<String, String>,
    sinks: BTreeMap<String, String>,
    operator: OperatorPod,
}

impl Fixture {
    fn new(extra: Value) -> Self {
        let spec = spec(extra);
        Self {
            rendered: rendered(&spec),
            spec,
            generated: BTreeMap::from([("main-fingerprint-key".into(), "k".repeat(64))]),
            sinks: BTreeMap::new(),
            operator: OperatorPod::named("weebo-notify"),
        }
    }

    fn input(&self) -> WorkloadInput<'_> {
        WorkloadInput {
            name: "main",
            namespace: "team",
            spec: &self.spec,
            rendered: &self.rendered,
            server_image: Some("ghcr.io/batleforc/weebo-notify:0.3.0"),
            migration: None,
            web_image: "ghcr.io/batleforc/weebo-notify-web:0.3.0",
            generated: &self.generated,
            sinks: &self.sinks,
            grow_claim: false,
            operator: &self.operator,
        }
    }
}

fn find<'a>(objects: &'a [OwnedObject], kind: &str, name: &str) -> Option<&'a OwnedObject> {
    objects.iter().find(|o| o.kind == kind && o.name == name)
}

fn env_of(container: &Value) -> BTreeMap<String, Value> {
    container["env"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap().to_string(), e.clone()))
        .collect()
}

#[test]
fn the_server_runs_its_file_its_secrets_and_only_the_boot_hash_rolls_it() {
    let fixture = Fixture::new(json!({}));
    let objects = objects(&fixture.input());
    let deployment = &find(&objects, "Deployment", "main").unwrap().manifest;
    let template = &deployment["spec"]["template"];
    let annotations = &template["metadata"]["annotations"];
    assert_eq!(
        annotations[api::annotations::BOOT_HASH],
        json!(fixture.rendered.boot_hash)
    );
    assert_eq!(
        annotations[IMAGE_ANNOTATION],
        "ghcr.io/batleforc/weebo-notify:0.3.0"
    );
    // The digest of the hot file is nowhere on the template.
    assert!(
        !template
            .to_string()
            .contains(&fixture.rendered.config_digest)
    );

    let container = &template["spec"]["containers"][0];
    let env = env_of(container);
    assert_eq!(env["WNK_CONFIG"]["value"], "/etc/weebo-notify/config.toml");
    assert_eq!(env["WNK_DATABASE__MIGRATE_ON_START"]["value"], "false");
    assert_eq!(
        env["WNK_DATABASE__URL"]["valueFrom"]["secretKeyRef"],
        json!({"name": "db", "key": "url", "optional": false})
    );
    assert_eq!(
        env["WNK_SINKS__FINGERPRINT_KEY"]["valueFrom"]["secretKeyRef"]["name"],
        "main-fingerprint-key"
    );
    // The ConfigMap as a directory (never a subPath), the VAPID key as a file.
    let mounts = container["volumeMounts"].as_array().unwrap();
    let config = mounts.iter().find(|m| m["name"] == "config").unwrap();
    assert_eq!(config["mountPath"], "/etc/weebo-notify");
    assert!(config.get("subPath").is_none());
    assert!(
        mounts
            .iter()
            .any(|m| m["mountPath"] == "/var/run/notify/vapid")
    );
    let volumes = template["spec"]["volumes"].as_array().unwrap();
    let vapid = volumes
        .iter()
        .find(|v| v["secret"]["secretName"] == "vapid")
        .unwrap();
    assert_eq!(
        vapid["secret"]["items"],
        json!([{"key": "key.pem", "path": "key"}])
    );
    // Probes on the server's own routes, restricted pods.
    assert_eq!(container["readinessProbe"]["httpGet"]["path"], "/readyz");
    assert_eq!(container["livenessProbe"]["httpGet"]["path"], "/healthz");
    assert_eq!(container["ports"][0]["containerPort"], 8080);
    assert_eq!(container["securityContext"]["readOnlyRootFilesystem"], true);
    assert_eq!(template["spec"]["securityContext"]["runAsUser"], NOTIFY_UID);
    assert_eq!(template["spec"]["automountServiceAccountToken"], false);
    // Two replicas by default: a PDB.
    assert!(find(&objects, "PodDisruptionBudget", "main").is_some());
    // The generated key: created once, readable by the pods.
    let key = find(&objects, "Secret", "main-fingerprint-key").unwrap();
    assert!(key.create_only);
    assert_eq!(
        key.manifest["metadata"]["labels"][api::CREDENTIAL_LABEL],
        "true"
    );
}

#[test]
fn a_pending_migration_runs_its_job_and_a_new_instance_waits_for_it() {
    let fixture = Fixture::new(json!({}));
    let target = "ghcr.io/batleforc/weebo-notify:0.3.0";
    let input = WorkloadInput {
        server_image: None,
        migration: Some(target),
        ..fixture.input()
    };
    let objects = objects(&input);
    assert!(find(&objects, "Deployment", "main").is_none());
    assert!(find(&objects, "PodDisruptionBudget", "main").is_none());
    let name = migrate_job("main", target);
    assert!(name.starts_with("main-migrate-") && name.len() == "main-migrate-".len() + 8);
    let job = find(&objects, "Job", &name).unwrap();
    assert!(job.create_only);
    let pod = &job.manifest["spec"]["template"]["spec"];
    assert_eq!(pod["restartPolicy"], "Never");
    let container = &pod["containers"][0];
    assert_eq!(container["args"], json!(["migrate"]));
    assert_eq!(container["image"], target);
    let env = env_of(container);
    assert!(env.contains_key("WNK_DATABASE__URL"));
    assert!(container.get("readinessProbe").is_none());
    // The same mounts as the server's.
    assert!(
        container["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["mountPath"] == "/etc/weebo-notify")
    );
    assert!(
        removed(&input)
            .iter()
            .any(|(_, kind, _)| *kind == "PodDisruptionBudget")
    );
    assert_ne!(migrate_job("main", target), migrate_job("main", "x:0.3.1"));
}

#[test]
fn the_network_policy_lets_in_the_operator_and_out_the_endpoints() {
    let fixture = Fixture::new(json!({
        "networkPolicy": {"ingressFrom": [{"namespaceSelector": {"matchLabels": {"name": "ingress"}}}]},
        "smtp": {"url": "smtp://relay.mail:2525", "from": "notify@example.com"},
    }));
    let objects = objects(&fixture.input());
    let policy = &find(&objects, "NetworkPolicy", "main").unwrap().manifest["spec"];
    assert_eq!(
        policy["podSelector"]["matchLabels"][SERVER_POD_LABEL],
        "true"
    );
    let from = policy["ingress"][0]["from"].as_array().unwrap();
    assert_eq!(from.len(), 2, "the given peer and the operator: {from:?}");
    assert_eq!(
        from[1]["podSelector"]["matchLabels"]["app.kubernetes.io/name"],
        "weebo-notify"
    );
    let ports: Vec<i64> = policy["egress"].as_array().unwrap().last().unwrap()["ports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["port"].as_i64().unwrap())
        .collect();
    assert_eq!(ports, [443, 2525, 5432, 9000]);
    // The web UI's: DNS only out.
    let web = &find(&objects, "NetworkPolicy", "main-web")
        .unwrap()
        .manifest["spec"];
    assert_eq!(web["egress"].as_array().unwrap().len(), 1);
}

#[test]
fn the_web_ui_gets_origins_and_a_read_only_root() {
    let fixture = Fixture::new(json!({}));
    let objects = objects(&fixture.input());
    let deployment = &find(&objects, "Deployment", "main-web").unwrap().manifest;
    let pod = &deployment["spec"]["template"]["spec"];
    let env = env_of(&pod["containers"][0]);
    assert_eq!(
        env["WNK_API_BASE_URL"]["value"],
        "https://notify.example.com"
    );
    assert_eq!(
        env["WNK_OIDC_ISSUER"]["value"],
        "https://id.example.com/application/o/notify/"
    );
    assert_eq!(env["WNK_OIDC_CLIENT_ID"]["value"], "notify");
    assert_eq!(
        env["WNK_ATTACHMENT_ORIGIN"]["value"],
        "http://rustfs.storage:9000"
    );
    assert_eq!(pod["securityContext"]["runAsUser"], WEB_UID);
    assert_eq!(
        pod["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
        true
    );
    assert_eq!(pod["initContainers"][0]["name"], "seed-html");
    assert!(find(&objects, "Service", "main-web").is_some());
    // The API's Ingress routes the API only.
    let api = &find(&objects, "Ingress", "main").unwrap().manifest["spec"];
    let paths = &api["rules"][0]["http"]["paths"];
    assert_eq!(paths.as_array().unwrap().len(), 1);
    assert_eq!(paths[0]["path"], "/api/v1");
    assert_eq!(api["tls"][0]["secretName"], "notify-tls");
    assert_eq!(api["ingressClassName"], "nginx");
    let web = &find(&objects, "Ingress", "main-web").unwrap().manifest["spec"];
    assert_eq!(
        web["rules"][0]["http"]["paths"][0]["backend"]["service"]["name"],
        "main-web"
    );
}

#[test]
fn what_the_spec_no_longer_wants_is_removed() {
    let fixture = Fixture::new(json!({
        "replicas": 1,
        "web": {"enabled": false},
        "serviceMonitor": {"enabled": false},
    }));
    let mut fixture = fixture;
    fixture.spec.ingress = None;
    let input = fixture.input();
    let objects = objects(&input);
    assert!(find(&objects, "Deployment", "main-web").is_none());
    assert!(find(&objects, "PodDisruptionBudget", "main").is_none());
    assert!(service_monitor(&input).is_none());
    let gone: Vec<String> = removed(&input)
        .into_iter()
        .map(|(_, kind, name)| format!("{kind}/{name}"))
        .collect();
    for expected in [
        "PodDisruptionBudget/main",
        "Deployment/main-web",
        "Service/main-web",
        "NetworkPolicy/main-web",
        "Ingress/main",
        "Ingress/main-web",
        "ServiceMonitor/main",
        "Secret/main-sinks",
    ] {
        assert!(
            gone.contains(&expected.to_string()),
            "{expected} in {gone:?}"
        );
    }
    // Without an Ingress, the UI points at the Service.
    assert_eq!(api_origin(&input), "http://main.team.svc:8080");
}

#[test]
fn the_service_monitor_scrapes_the_server_service() {
    let fixture = Fixture::new(json!({"serviceMonitor": {"labels": {"release": "kps"}}}));
    let monitor = service_monitor(&fixture.input()).unwrap();
    assert_eq!(monitor.api_version, "monitoring.coreos.com/v1");
    assert_eq!(monitor.manifest["metadata"]["labels"]["release"], "kps");
    assert_eq!(
        monitor.manifest["spec"]["selector"]["matchLabels"],
        json!(selector("main", "server"))
    );
    assert_eq!(monitor.manifest["spec"]["endpoints"][0]["path"], "/metrics");
}

#[test]
fn a_filesystem_store_is_a_claim_recreated_pods_and_a_link_key() {
    let mut fixture = Fixture::new(json!({
        "replicas": 1,
        "attachments": {"filesystem": {"publicBaseUrl": "https://notify.example.com", "size": "10Gi"}},
    }));
    fixture.sinks = BTreeMap::from([("sink-0123".into(), "s3cr3t".into())]);
    let input = fixture.input();
    let objects = objects(&input);
    let claim = find(&objects, "PersistentVolumeClaim", "main-attachments").unwrap();
    assert!(claim.create_only);
    assert_eq!(
        claim.manifest["spec"]["resources"]["requests"]["storage"],
        "10Gi"
    );
    let grown = objects_with_growth(&fixture);
    assert!(
        !find(&grown, "PersistentVolumeClaim", "main-attachments")
            .unwrap()
            .create_only
    );
    let deployment = &find(&objects, "Deployment", "main").unwrap().manifest;
    assert_eq!(deployment["spec"]["strategy"]["type"], "Recreate");
    let container = &deployment["spec"]["template"]["spec"]["containers"][0];
    let env = env_of(container);
    assert_eq!(
        env["WNK_ATTACHMENTS__TOKEN_SECRET"]["valueFrom"]["secretKeyRef"]["name"],
        "main-attachments-key"
    );
    let sinks = find(&objects, "Secret", "main-sinks").unwrap();
    assert!(!sinks.create_only);
    assert!(sinks.manifest["data"]["sink-0123"].is_string());
    assert!(claim_grows(&fixture.spec, Some("5Gi")));
    assert!(!claim_grows(&fixture.spec, Some("10Gi")));
    assert!(!claim_grows(&fixture.spec, Some("20G")));
    assert_eq!(quantity_bytes("1Ki"), Some(1024));
    assert_eq!(quantity_bytes("1.5Gi"), None);
}

fn objects_with_growth(fixture: &Fixture) -> Vec<OwnedObject> {
    objects(&WorkloadInput {
        grow_claim: true,
        ..fixture.input()
    })
}

#[test]
fn images_default_to_the_targeted_version() {
    let default = "ghcr.io/batleforc/weebo-notify:0.3.0";
    assert_eq!(image(&Default::default(), default), default);
    let tagged = api::local_instance::Image {
        tag: Some("0.3.1".into()),
        ..Default::default()
    };
    assert_eq!(
        image(&tagged, default),
        "ghcr.io/batleforc/weebo-notify:0.3.1"
    );
    assert_eq!(tag("registry:5000/notify:0.3.0"), "0.3.0");
    assert_eq!(tag("registry:5000/notify"), "latest");
    assert_eq!(
        origin("https://id.example.com/a/b/"),
        Some("https://id.example.com".into())
    );
}
