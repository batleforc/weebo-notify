//! `NotifyLocalInstance` (namespaced): the instance's configuration and
//! its workload (`reconcile_instance`). Watched besides the CR, by
//! metadata: its `ConfigMap`, `Deployment`s (a rollout's progress) and
//! migration `Job`s; and the channels, service accounts, transforms and trust policies bound
//! to it — a change of one re-renders it. No watch of the Secrets it
//! references: they are checked again on every pass.
//!
//! A finalizer holds its deletion; the objects it owns go with it (owner
//! references).

use std::sync::Arc;
use std::time::Instant;

use api::LocalInstance;
use application::ports::Owner;
use application::use_cases::reconcile_instance::{
    InstancePass, InstancePorts, reconcile_instance, status_fields,
};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::Api;
use kube::core::PartialObjectMeta;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, Resource, ResourceExt};
use weebo_kit_runtime::events;
use weebo_kit_runtime::shared::WatchesIn;
use weebo_kit_runtime::status::StatusUpdate;

use super::object::BoundObject;
use super::{Ctx, Error, error_policy};

const KIND: &str = "NotifyLocalInstance";

/// The instance an object of it belongs to (`app.kubernetes.io/instance`
/// with `managed-by: weebo-notify`).
fn instance_of<K: Resource>(object: &K) -> Option<ObjectRef<LocalInstance>> {
    let labels = object.meta().labels.as_ref()?;
    (labels
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        == Some(api::MANAGED_BY))
    .then_some(())?;
    let name = labels.get("app.kubernetes.io/instance")?;
    Some(ObjectRef::new(name).within(object.meta().namespace.as_deref()?))
}

/// What of an object the instance renders: its spec (generation), its
/// labels (the instance's selector), its deletion and the instance it is
/// bound to — not its conditions, which it writes on every pass.
fn bound_change<K: BoundObject>(object: &K) -> Option<u64> {
    let meta = object.meta();
    super::hash_of(&(
        meta.generation,
        &meta.labels,
        meta.deletion_timestamp.is_some(),
        object.bound_instance(),
    ))
}

/// The instance an object is bound to.
fn bound_instance<K: BoundObject>(object: K) -> Option<ObjectRef<LocalInstance>> {
    let bound = object.bound_instance()?;
    Some(ObjectRef::new(&bound.name).within(bound.namespace.as_deref()?))
}

/// `controller` woken by every change of the objects of `K` bound to an
/// instance.
fn watching<K: BoundObject>(
    controller: Controller<LocalInstance>,
    ctx: &Ctx,
    client: &Client,
) -> Controller<LocalInstance> {
    controller.watches_in_filtered(
        &ctx.watches,
        &ctx.scope,
        client,
        bound_change::<K>,
        bound_instance::<K>,
    )
}

pub async fn run(client: Client, ctx: Arc<Ctx>) {
    let health = ctx.health.clone();
    let owned = watcher::Config::default()
        .labels(&format!("app.kubernetes.io/managed-by={}", api::MANAGED_BY));
    // Metadata only: what changed, not the objects.
    let config_maps: Api<PartialObjectMeta<ConfigMap>> = ctx.scope.api(client.clone());
    let deployments: Api<PartialObjectMeta<Deployment>> = ctx.scope.api(client.clone());
    let jobs: Api<PartialObjectMeta<Job>> = ctx.scope.api(client.clone());
    let controller = ctx
        .watches
        .controller(ctx.scope.api::<LocalInstance>(client.clone()), &ctx.scope)
        // Its configuration edited or deleted by hand: written back.
        .watches_stream(
            watcher(config_maps, owned.clone())
                .default_backoff()
                .touched_objects(),
            |c| instance_of(&c),
        )
        // A rollout's progress, a migration finishing.
        .watches_stream(
            watcher(deployments, owned.clone())
                .default_backoff()
                .touched_objects(),
            |d| instance_of(&d),
        )
        .watches_stream(
            watcher(jobs, owned).default_backoff().touched_objects(),
            |j| instance_of(&j),
        );
    let controller = watching::<api::Channel>(controller, &ctx, &client);
    let controller = watching::<api::ServiceAccount>(controller, &ctx, &client);
    let controller = watching::<api::Transform>(controller, &ctx, &client);
    let controller = watching::<api::TrustPolicy>(controller, &ctx, &client);
    let results =
        controller
            .graceful_shutdown_on(ctx.shutdown.wait())
            .run(reconcile, error_policy, ctx);
    super::drive(KIND, health, results).await;
}

async fn reconcile(obj: Arc<LocalInstance>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let namespace = obj.namespace().unwrap_or_default();
    let api: Api<LocalInstance> = Api::namespaced(ctx.client.clone(), &namespace);
    finalizer(&api, super::FINALIZER, obj, |event| async {
        match event {
            Finalizer::Apply(instance) => apply(&api, &instance, &ctx).await,
            Finalizer::Cleanup(instance) => {
                // Its objects go with it (owner references): nothing to
                // undo. TODO(2c): hold it while objects are bound to it.
                super::finalize(instance.as_ref(), &ctx, async { Ok(()) }).await
            }
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

async fn apply(
    api: &Api<LocalInstance>,
    instance: &LocalInstance,
    ctx: &Ctx,
) -> Result<Action, Error> {
    let started = Instant::now();
    let owner = Owner {
        namespace: instance.namespace().unwrap_or_default(),
        name: instance.name_any(),
        uid: instance.uid().unwrap_or_default(),
    };
    let previous = instance.status.clone().unwrap_or_default();
    let report = reconcile_instance(
        InstancePass {
            owner: &owner,
            spec: &instance.spec,
            annotations: instance.annotations(),
            status: &previous,
            default_image: &ctx.settings.default_image,
            default_web_image: &ctx.settings.default_web_image,
            operator: &ctx.operator,
        },
        &InstancePorts {
            objects: ctx.objects.as_ref(),
            credentials: ctx.credentials.as_ref(),
            ledger: ctx.ledger.as_ref(),
            directory: ctx.directory.as_ref(),
            jobs: ctx.jobs.as_ref(),
            keys: ctx.keys.as_ref(),
        },
    )
    .await;
    super::record_reconcile(KIND, started, &report.outcome);
    weebo_kit_runtime::status::publish(
        api,
        &ctx.recorder,
        super::FIELD_MANAGER,
        instance,
        &previous.conditions,
        StatusUpdate {
            ready: &report.outcome.ready,
            advisories: &report.outcome.advisories,
            fields: status_fields(&previous, &report.status),
        },
    )
    .await?;
    for (reason, message) in &report.events {
        events::announce_change(
            &ctx.recorder,
            &instance.object_ref(&()),
            *reason,
            events::RECONCILE,
            message,
        )
        .await;
    }
    Ok(super::requeue_after(instance, &report.outcome))
}
