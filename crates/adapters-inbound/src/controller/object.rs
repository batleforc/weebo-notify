//! `NotifyChannel`, `NotifyServiceAccount`, `NotifyTransform`,
//! `NotifyTrustPolicy` (namespaced): bound to their instance, and what the
//! instance made of them reflected (`reconcile_object`). One controller
//! per kind, the same code ([`BoundObject`]). Triggered by the object and
//! by its instance (its `status.objects` names what it rendered or
//! refused). They write nothing but their status: the instance renders
//! them, its controller woken by their changes
//! (`local_instance::run`).

use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;

use api::LocalInstance;
use api::status::ObjectStatus;
use application::objects::ObjectKind;
use application::use_cases::reconcile_object::{ObjectPass, reconcile_object, release_object};
use k8s_openapi::NamespaceResourceScope;
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::{Client, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use weebo_kit_api::{BoundInstance, InstanceRef};
use weebo_kit_runtime::shared::WatchesIn;
use weebo_kit_runtime::status::StatusUpdate;

use super::{Ctx, Error, error_policy};

/// What the controller needs of a kind bound to an instance.
pub trait BoundObject:
    Resource<DynamicType = (), Scope = NamespaceResourceScope>
    + Clone
    + Debug
    + DeserializeOwned
    + Serialize
    + Send
    + Sync
    + 'static
{
    const KIND: ObjectKind;
    fn spec_id(&self) -> Option<&str>;
    fn instance_ref(&self) -> Option<&InstanceRef>;
    fn object_status(&self) -> Option<&ObjectStatus>;

    /// The instance recorded at the last resolution.
    fn bound_instance(&self) -> Option<&BoundInstance> {
        self.object_status()?.instance.as_ref()
    }
}

macro_rules! bound_object {
    ($kind:ty, $variant:ident, $id:expr) => {
        impl BoundObject for $kind {
            const KIND: ObjectKind = ObjectKind::$variant;
            fn spec_id(&self) -> Option<&str> {
                let id: fn(&Self) -> Option<&str> = $id;
                id(self)
            }
            fn instance_ref(&self) -> Option<&InstanceRef> {
                self.spec.instance_ref.as_ref()
            }
            fn object_status(&self) -> Option<&ObjectStatus> {
                self.status.as_ref()
            }
        }
    };
}

bound_object!(api::Channel, Channel, |c| c.spec.id.as_deref());
bound_object!(api::ServiceAccount, ServiceAccount, |s| s
    .spec
    .id
    .as_deref());
bound_object!(api::Transform, Transform, |t| t.spec.id.as_deref());
bound_object!(api::TrustPolicy, TrustPolicy, |_| None);

/// The objects of kind `K` an instance's render map names.
fn rendered_by<K: BoundObject>(instance: &LocalInstance) -> Vec<ObjectRef<K>> {
    let prefix = format!("{}/", K::KIND.kind());
    instance
        .status
        .iter()
        .flat_map(|s| s.objects.keys())
        .filter_map(|key| {
            let (ns, name) = key.strip_prefix(&prefix)?.split_once('/')?;
            Some(ObjectRef::<K>::new(name).within(ns))
        })
        .collect()
}

pub async fn run<K: BoundObject>(client: Client, ctx: Arc<Ctx>) {
    let health = ctx.health.clone();
    let results = ctx
        .watches
        .controller(ctx.scope.api::<K>(client.clone()), &ctx.scope)
        // Only when what the instance made of its objects changes: its
        // other status writes would wake them all for nothing.
        .watches_in_filtered(
            &ctx.watches,
            &ctx.scope,
            &client,
            |instance: &LocalInstance| {
                super::hash_of(&instance.status.as_ref().map(|s| &s.objects))
            },
            |instance| rendered_by::<K>(&instance),
        )
        .graceful_shutdown_on(ctx.shutdown.wait())
        .run(reconcile::<K>, error_policy, ctx);
    super::drive(K::KIND.kind(), health, results).await;
}

async fn reconcile<K: BoundObject>(obj: Arc<K>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let namespace = obj.namespace().unwrap_or_default();
    let api: Api<K> = Api::namespaced(ctx.client.clone(), &namespace);
    finalizer(&api, super::FINALIZER, obj, |event| async {
        match event {
            Finalizer::Apply(object) => apply(&api, object.as_ref(), &ctx).await,
            Finalizer::Cleanup(object) => {
                let released = release_object(
                    K::KIND,
                    &namespace,
                    &object.name_any(),
                    object.bound_instance(),
                    ctx.attached.as_ref(),
                )
                .await;
                if let Err(failure) = &released {
                    // Said on the object too, not only in an Event.
                    let previous = object
                        .object_status()
                        .map(|s| s.conditions.as_slice())
                        .unwrap_or_default();
                    weebo_kit_runtime::status::publish(
                        &api,
                        &ctx.recorder,
                        super::FIELD_MANAGER,
                        object.as_ref(),
                        previous,
                        StatusUpdate {
                            ready: &Err(failure.clone()),
                            advisories: &[],
                            fields: serde_json::Map::new(),
                        },
                    )
                    .await?;
                }
                super::finalize(object.as_ref(), &ctx, async {
                    released.map_err(Error::Cleanup)
                })
                .await
            }
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

async fn apply<K: BoundObject>(api: &Api<K>, obj: &K, ctx: &Ctx) -> Result<Action, Error> {
    let started = Instant::now();
    let namespace = obj.namespace().unwrap_or_default();
    let name = obj.name_any();
    let report = reconcile_object(
        ObjectPass {
            kind: K::KIND,
            namespace: &namespace,
            name: &name,
            generation: obj.meta().generation,
            spec_id: obj.spec_id(),
            instance_ref: obj.instance_ref(),
            recorded: obj.bound_instance(),
        },
        ctx.attached.as_ref(),
    )
    .await;
    super::record_reconcile(K::KIND.kind(), started, &report.outcome);
    let mut fields = serde_json::Map::new();
    if let Some(instance) = &report.outcome.instance {
        #[expect(clippy::expect_used, reason = "a BoundInstance is a struct of strings")]
        let instance = serde_json::to_value(instance).expect("a BoundInstance serializes");
        fields.insert("instance".into(), instance);
    }
    fields.insert(
        "id".into(),
        report
            .id
            .clone()
            .map_or(serde_json::Value::Null, Into::into),
    );
    let previous = obj
        .object_status()
        .map(|s| s.conditions.as_slice())
        .unwrap_or_default();
    weebo_kit_runtime::status::publish(
        api,
        &ctx.recorder,
        super::FIELD_MANAGER,
        obj,
        previous,
        StatusUpdate {
            ready: &report.outcome.ready,
            advisories: &report.outcome.advisories,
            fields,
        },
    )
    .await?;
    Ok(super::requeue_waiting(obj, &report.outcome))
}

#[cfg(test)]
mod tests {
    use api::status::{InstanceStatus, ObjectRender, RenderState};

    use super::*;

    #[test]
    fn an_instance_wakes_the_objects_of_each_kind_it_rendered() {
        let render = ObjectRender {
            id: None,
            state: RenderState::Rendered,
            message: None,
            conflict_with: None,
            unmanaged_channels: Vec::new(),
            generation: None,
            applied_generation: None,
        };
        let mut status = InstanceStatus::default();
        for key in [
            "NotifyChannel/team/alerts",
            "NotifyTransform/team/grafana",
            "NotifyChannel/ops/pages",
        ] {
            status.objects.insert(key.into(), render.clone());
        }
        let spec = serde_json::from_value(serde_json::json!({
            "database": {"urlSecretRef": {"name": "db", "key": "url"}},
            "attachments": {"s3": {"endpoint": "http://s3", "bucket": "b",
                                   "credentialsSecretRef": {"name": "s3"}}},
            "oidc": {"issuer": "https://id.example", "clientId": "notify"},
        }))
        .unwrap();
        let mut instance = LocalInstance::new("main", spec);
        instance.status = Some(status);
        let channels = rendered_by::<api::Channel>(&instance);
        assert_eq!(
            channels,
            [
                ObjectRef::new("pages").within("ops"),
                ObjectRef::new("alerts").within("team"),
            ]
        );
        assert_eq!(rendered_by::<api::Transform>(&instance).len(), 1);
        assert!(rendered_by::<api::TrustPolicy>(&instance).is_empty());
    }
}
