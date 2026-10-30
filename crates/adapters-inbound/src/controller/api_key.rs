//! `NotifyApiKey` (namespaced): its key minted through the instance's key
//! issuer and written to its targets (`reconcile_api_key`); revoked, its
//! targets deleted, on deletion. Triggered by the key, by the
//! `NotifyServiceAccount` it names (its readiness, its id) and by its
//! instance (running, rolled out with its key issuer).

use std::sync::Arc;
use std::time::Instant;

use api::{ApiKey, LocalInstance};
use application::use_cases::reconcile_api_key::{
    ApiKeyPass, ApiKeyPorts, ApiKeyRelease, reconcile_api_key, release_api_key,
};
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::{ObjectRef, Store};
use kube::{Client, Resource, ResourceExt};
use weebo_kit_runtime::events;
use weebo_kit_runtime::shared::WatchesIn;
use weebo_kit_runtime::status::StatusUpdate;

use super::{Ctx, Error, error_policy};

/// The keys of `account`'s namespace naming it.
fn naming(keys: Option<&Store<ApiKey>>, account: &api::ServiceAccount) -> Vec<ObjectRef<ApiKey>> {
    let (namespace, name) = (account.namespace(), account.name_any());
    keys.map(|store| store.state())
        .unwrap_or_default()
        .iter()
        .filter(|k| k.namespace() == namespace && k.spec.service_account_ref.name == name)
        .map(|k| ObjectRef::from_obj(k.as_ref()))
        .collect()
}

/// The keys bound to `instance`.
fn bound_to(keys: Option<&Store<ApiKey>>, instance: &LocalInstance) -> Vec<ObjectRef<ApiKey>> {
    let (namespace, name) = (instance.namespace(), instance.name_any());
    keys.map(|store| store.state())
        .unwrap_or_default()
        .iter()
        .filter(|k| {
            k.status
                .as_ref()
                .and_then(|s| s.instance.as_ref())
                .is_some_and(|b| b.namespace == namespace && b.name == name)
        })
        .map(|k| ObjectRef::from_obj(k.as_ref()))
        .collect()
}

/// What a key waits for of its instance (its `Service`, its boot
/// configuration rolled out), hashed by the watch predicate.
fn instance_view(instance: &LocalInstance) -> Option<serde_json::Value> {
    let status = instance.status.as_ref()?;
    let ready = weebo_kit_api::status::ready(&status.conditions).map(|c| &c.status);
    serde_json::to_value((&status.endpoint, &status.boot_hash, ready)).ok()
}

pub async fn run(client: Client, ctx: Arc<Ctx>) {
    let health = ctx.health.clone();
    let keys = ctx.watches.get::<ApiKey>().map(|w| w.store());
    let keys_of_instance = keys.clone();
    let results = ctx
        .watches
        .controller(ctx.scope.api::<ApiKey>(client.clone()), &ctx.scope)
        // Only when what a key reads of its account changes.
        .watches_in_filtered(
            &ctx.watches,
            &ctx.scope,
            &client,
            |account: &api::ServiceAccount| super::hash_of(&account_view(account)),
            move |account| naming(keys.as_ref(), &account),
        )
        .watches_in_filtered(
            &ctx.watches,
            &ctx.scope,
            &client,
            |instance: &LocalInstance| super::hash_of(&instance_view(instance)),
            move |instance| bound_to(keys_of_instance.as_ref(), &instance),
        )
        .graceful_shutdown_on(ctx.shutdown.wait())
        .run(reconcile, error_policy, ctx);
    super::drive(api::kinds::API_KEY, health, results).await;
}

/// What a key reads of its account (its id, instance and `Ready`),
/// hashed by the watch predicate.
fn account_view(account: &api::ServiceAccount) -> Option<serde_json::Value> {
    let status = account.status.as_ref()?;
    let ready = weebo_kit_api::status::ready(&status.conditions).map(|c| &c.status);
    serde_json::to_value((&status.id, &status.instance, ready)).ok()
}

fn ports<'a>(ctx: &'a Ctx, store: &'a dyn application::ports::SecretStore) -> ApiKeyPorts<'a> {
    ApiKeyPorts {
        instances: ctx.attached.as_ref(),
        accounts: ctx.accounts.as_ref(),
        objects: ctx.objects.as_ref(),
        issuer: ctx.issuer.as_ref(),
        secrets: store,
    }
}

async fn reconcile(obj: Arc<ApiKey>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let namespace = obj.namespace().unwrap_or_default();
    let api: Api<ApiKey> = Api::namespaced(ctx.client.clone(), &namespace);
    finalizer(&api, super::FINALIZER, obj, |event| async {
        match event {
            Finalizer::Apply(key) => apply(&api, key.as_ref(), &ctx).await,
            Finalizer::Cleanup(key) => {
                let name = key.name_any();
                let store = ctx.secret_stores.store();
                let released = release_api_key(
                    ApiKeyRelease {
                        namespace: &namespace,
                        name: &name,
                        spec: &key.spec,
                        recorded: key.status.as_ref().and_then(|s| s.instance.as_ref()),
                        orphaned: api::ANNOTATIONS.force_orphan(key.meta()),
                    },
                    ports(&ctx, store.as_ref()),
                )
                .await;
                if let Err(failure) = &released {
                    let previous = key
                        .status
                        .as_ref()
                        .map(|s| s.conditions.as_slice())
                        .unwrap_or_default();
                    weebo_kit_runtime::status::publish(
                        &api,
                        &ctx.recorder,
                        super::FIELD_MANAGER,
                        key.as_ref(),
                        previous,
                        StatusUpdate {
                            ready: &Err(failure.clone()),
                            advisories: &[],
                            fields: serde_json::Map::new(),
                        },
                    )
                    .await?;
                }
                super::finalize(key.as_ref(), &ctx, async {
                    released.map_err(Error::Cleanup)
                })
                .await
            }
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

async fn apply(api: &Api<ApiKey>, key: &ApiKey, ctx: &Ctx) -> Result<Action, Error> {
    let started = Instant::now();
    let namespace = key.namespace().unwrap_or_default();
    let name = key.name_any();
    let status = key.status.clone().unwrap_or_default();
    let rotate = api::ANNOTATIONS.rotate_nonce(key.meta());
    let store = ctx.secret_stores.store();
    let report = reconcile_api_key(
        ApiKeyPass {
            namespace: &namespace,
            name: &name,
            spec: &key.spec,
            status: &status,
            rotate: rotate.as_deref(),
        },
        ports(ctx, store.as_ref()),
    )
    .await;
    super::record_reconcile(api::kinds::API_KEY, started, &report.outcome);
    let mut fields = serde_json::Map::new();
    if let Some(instance) = &report.outcome.instance {
        #[expect(clippy::expect_used, reason = "a BoundInstance is a struct of strings")]
        let instance = serde_json::to_value(instance).expect("a BoundInstance serializes");
        fields.insert("instance".into(), instance);
    }
    for (field, value) in [
        ("serviceAccount", &report.service_account),
        ("keyId", &report.key_id),
        ("keyPrefix", &report.key_prefix),
        ("lastRotation", &report.last_rotation),
    ] {
        fields.insert(
            field.into(),
            value.clone().map_or(serde_json::Value::Null, Into::into),
        );
    }
    let previous = status.conditions.as_slice();
    weebo_kit_runtime::status::publish(
        api,
        &ctx.recorder,
        super::FIELD_MANAGER,
        key,
        previous,
        StatusUpdate {
            ready: &report.outcome.ready,
            advisories: &report.outcome.advisories,
            fields,
        },
    )
    .await?;
    for (reason, message) in &report.events {
        events::announce_change(
            &ctx.recorder,
            &key.object_ref(&()),
            *reason,
            events::RECONCILE,
            message,
        )
        .await;
    }
    Ok(super::requeue_after(key, &report.outcome))
}
