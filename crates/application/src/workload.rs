//! The objects an instance owns, rendered from its spec: for now its
//! configuration's `ConfigMap`.
//!
//! TODO(2b): the server's `Deployment`, `Service`, `ServiceAccount`,
//! `NetworkPolicy` and `PodDisruptionBudget`, the migration `Job`, the web
//! UI's `Deployment`/`Service`, the `Ingress`es, the `ServiceMonitor`, the
//! filesystem store's claim, the generated keys and `<name>-sinks` (the
//! sinks' secrets, copied) — from [`crate::render::Rendered`]'s `env` and
//! `files`.

use std::collections::BTreeMap;

use api::local_instance::Names;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use crate::ports::{OwnedObject, Owner};
use crate::render::paths;

/// The labels of every object of an instance: `managed-by` and `instance`
/// hand an existing object over to it (the kit's `is_ours`).
pub fn labels(instance: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), "notify".to_string()),
        (
            "app.kubernetes.io/instance".to_string(),
            instance.to_string(),
        ),
        (
            "app.kubernetes.io/managed-by".to_string(),
            api::MANAGED_BY.to_string(),
        ),
    ])
}

/// `<name>-config`, holding `config.toml`: rewritten in place, Notify
/// reloads it when it changes (`[reload] watch`).
pub fn config_map(owner: &Owner, text: &str) -> OwnedObject {
    let mut labels = labels(&owner.name);
    labels.insert("app.kubernetes.io/component".into(), "config".into());
    let map = ConfigMap {
        metadata: ObjectMeta {
            name: Some(Names(&owner.name).config()),
            namespace: Some(owner.namespace.clone()),
            labels: Some(labels),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(
            paths::CONFIG_KEY.to_string(),
            text.to_string(),
        )])),
        ..Default::default()
    };
    OwnedObject::of(&map, false)
}
