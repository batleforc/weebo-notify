//! A `NotifyApiKey`: a key of a managed service account, minted through
//! its instance's key issuer (ADR-0033) and written to the CR's secret
//! targets (Kubernetes `Secret`s or Vault, the kit's `SecretStore`).
//!
//! A key's secret can't be read back, so a new key is minted when none
//! fits: no key recorded, its account, scopes or channels changed, a
//! target lost it, or a rotation was asked. The new key is written to
//! every target before any other key of the CR is revoked. The keys of a
//! CR are those carrying its label ([`api::api_key::label`]): one minted
//! by a pass that failed half-way is revoked by the next.

use std::collections::BTreeMap;

use api::api_key::{ApiKeySpec, ApiKeyStatus, keys, label};
use api::local_instance::Names;
use domain::Failure;
use domain::error::ReasonCode;
use weebo_kit_application::emit;

use super::{Outcome, api_failure, instance_failure, workload_failure};
use crate::ports::{
    AttachedInstances, BoundInstance, IssuedKey, KeyIssuer, NewKey, NotifyApiError, OwnedObjects,
    Owner, SecretOwner, SecretStore, SecretStoreError, ServiceAccounts,
};
use crate::render::paths;

/// What a pass is about.
#[derive(Debug, Clone, Copy)]
pub struct ApiKeyPass<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
    pub spec: &'a ApiKeySpec,
    pub status: &'a ApiKeyStatus,
    /// `notify.weebo.io/rotate-secret`.
    pub rotate: Option<&'a str>,
}

/// What a pass needs.
#[derive(Clone, Copy)]
pub struct ApiKeyPorts<'a> {
    pub instances: &'a dyn AttachedInstances,
    pub accounts: &'a dyn ServiceAccounts,
    /// Where the instance's key issuer token is.
    pub objects: &'a dyn OwnedObjects,
    pub issuer: &'a dyn KeyIssuer,
    pub secrets: &'a dyn SecretStore,
}

/// The outcome, and the status fields of the key.
#[derive(Debug, Clone)]
pub struct ApiKeyReport {
    pub outcome: Outcome,
    pub service_account: Option<String>,
    pub key_id: Option<String>,
    pub key_prefix: Option<String>,
    pub last_rotation: Option<String>,
    /// Events to announce besides the outcome (`KeyRotated`).
    pub events: Vec<(ReasonCode, String)>,
}

fn notify_failure(e: NotifyApiError) -> Failure {
    let reason = match e {
        NotifyApiError::Refused(_) => ReasonCode::KeyRefused,
        NotifyApiError::Unauthorized(_) | NotifyApiError::Failed(_) => {
            ReasonCode::KeyIssuerUnavailable
        }
    };
    Failure::new(reason, e.to_string())
}

fn secret_failure(e: impl std::fmt::Display) -> Failure {
    Failure::new(ReasonCode::SecretTargetFailed, e.to_string())
}

/// The bearer token of the operator's `[[key_issuers]]` entry.
fn bearer(secret: &str) -> String {
    format!("wki_{}_{secret}", paths::KEY_ISSUER_ID)
}

fn sorted<T: Ord + Clone>(items: &[T]) -> Vec<T> {
    let mut items = items.to_vec();
    items.sort();
    items.dedup();
    items
}

/// Whether `key` is what the spec asks for.
fn fits(key: &IssuedKey, account: &str, spec: &ApiKeySpec) -> bool {
    key.service_account.as_deref() == Some(account)
        && sorted(&key.scopes) == sorted(&spec.scopes)
        && sorted(&key.channels) == sorted(&spec.channels)
}

/// The owner a key's targets are recorded for.
fn owner<'a>(namespace: &'a str, name: &'a str) -> SecretOwner<'a> {
    SecretOwner::namespaced(api::kinds::API_KEY, namespace, name)
}

/// The instance's Notify API and its key issuer's bearer token: `None`
/// until the instance runs and generated the token.
async fn issuer_of(
    instance: &api::LocalInstance,
    bound: &BoundInstance,
    objects: &dyn OwnedObjects,
) -> Result<Option<(String, String)>, Failure> {
    let Some(endpoint) = instance.status.as_ref().and_then(|s| s.endpoint.clone()) else {
        return Ok(None);
    };
    let owner = Owner {
        namespace: bound.namespace.clone().unwrap_or_default(),
        name: bound.name.clone(),
        uid: instance.metadata.uid.clone().unwrap_or_default(),
    };
    let token = objects
        .secret_text(
            &owner,
            &Names(&bound.name).key_issuer(),
            paths::GENERATED_KEY,
        )
        .await
        .map_err(workload_failure)?;
    Ok(token.map(|t| (endpoint, bearer(&t))))
}

pub async fn reconcile_api_key(pass: ApiKeyPass<'_>, ports: ApiKeyPorts<'_>) -> ApiKeyReport {
    let mut report = ApiKeyReport {
        outcome: Outcome::synced(""),
        service_account: pass.status.service_account.clone(),
        key_id: pass.status.key_id.clone(),
        key_prefix: pass.status.key_prefix.clone(),
        last_rotation: pass.status.last_rotation.clone(),
        events: Vec::new(),
    };
    let bound = match ports
        .instances
        .resolve_attached(
            pass.namespace,
            pass.spec.instance_ref.as_ref(),
            pass.status.instance.as_ref(),
        )
        .await
    {
        Ok(bound) => bound,
        Err(e) => {
            report.outcome = Outcome::failed(instance_failure(e));
            return report;
        }
    };
    report.outcome = match run(&pass, &ports, &bound, &mut report).await {
        Ok(message) => Outcome::synced(message),
        Err(failure) => Outcome::failed(failure),
    }
    .on(bound);
    report
}

async fn run(
    pass: &ApiKeyPass<'_>,
    ports: &ApiKeyPorts<'_>,
    bound: &BoundInstance,
    report: &mut ApiKeyReport,
) -> Result<String, Failure> {
    let (ns, name, spec) = (pass.namespace, pass.name, pass.spec);
    let instance = ports
        .instances
        .instance(bound)
        .await
        .map_err(api_failure)?
        .ok_or_else(|| Failure::new(ReasonCode::InstanceNotFound, format!("{bound} is gone")))?;

    // The account, on the same instance.
    let account_name = &spec.service_account_ref.name;
    let not_ready = |why: String| Failure::new(ReasonCode::ServiceAccountNotReady, why);
    let account = ports
        .accounts
        .get(ns, account_name)
        .await
        .map_err(api_failure)?
        .ok_or_else(|| not_ready(format!("no NotifyServiceAccount {ns}/{account_name}")))?;
    if account.instance.as_ref() != Some(bound) {
        return Err(not_ready(format!(
            "NotifyServiceAccount {ns}/{account_name} is not bound to {bound}"
        )));
    }
    let account_id = account.id.clone().ok_or_else(|| {
        not_ready(format!(
            "NotifyServiceAccount {ns}/{account_name} has no id yet"
        ))
    })?;
    report.service_account = Some(account_id.clone());

    // The targets, in reach.
    let owner = owner(ns, name);
    let targets = emit::targets(&spec.secret_targets, name, ports.secrets);
    emit::check_targets(&targets, &owner).map_err(secret_failure)?;

    // The issuer, and this CR's keys.
    let (endpoint, token) = issuer_of(&instance, bound, ports.objects)
        .await?
        .ok_or_else(|| {
            Failure::new(
                ReasonCode::KeyIssuerUnavailable,
                format!("waiting for {bound} to run with its key issuer"),
            )
        })?;
    let label = label(ns, name);
    let ours: Vec<IssuedKey> = ports
        .issuer
        .list(&endpoint, &token)
        .await
        .map_err(notify_failure)?
        .into_iter()
        .filter(|k| !k.revoked && k.label.as_deref() == Some(label.as_str()))
        .collect();
    let current = pass
        .status
        .key_id
        .as_deref()
        .and_then(|id| ours.iter().find(|k| k.id == id))
        .filter(|k| fits(k, &account_id, spec));
    let rotate =
        emit::rotation_requested(&pass.rotate.map(String::from), &pass.status.last_rotation);
    let lost = emit::any_missing(&targets, &owner, ports.secrets)
        .await
        .map_err(secret_failure)?;

    let keep = match current {
        Some(key) if !rotate && !lost => key.clone(),
        _ => {
            if !account.ready {
                return Err(not_ready(format!(
                    "waiting for every pod of {bound} to apply NotifyServiceAccount \
                     {ns}/{account_name}"
                )));
            }
            let minted = ports
                .issuer
                .mint(
                    &endpoint,
                    &token,
                    &NewKey {
                        service_account: account_id.clone(),
                        label: label.clone(),
                        scopes: sorted(&spec.scopes),
                        channels: sorted(&spec.channels),
                    },
                )
                .await
                .map_err(notify_failure)?;
            let data = BTreeMap::from([
                (keys::API_KEY.to_string(), minted.secret.clone()),
                (keys::URL.to_string(), public_url(&instance, &endpoint)),
            ]);
            if let Err(e) = emit::write_all(&targets, &owner, &data, ports.secrets).await {
                return Err(unwritten(ports, &endpoint, &token, &minted.key.id, &e).await);
            }
            if pass.status.key_id.is_some() {
                report.events.push((
                    ReasonCode::KeyRotated,
                    format!(
                        "key {} written to {} target(s), the previous one revoked",
                        minted.key.prefix,
                        targets.len()
                    ),
                ));
            }
            report.key_id = Some(minted.key.id.clone());
            report.key_prefix = Some(minted.key.prefix.clone());
            report.last_rotation = pass.rotate.map(String::from);
            minted.key
        }
    };
    // Superseded, or minted by a pass that failed before recording it.
    for key in ours.iter().filter(|k| k.id != keep.id) {
        ports
            .issuer
            .revoke(&endpoint, &token, &key.id)
            .await
            .map_err(notify_failure)?;
    }
    Ok(format!(
        "key {} of service account {account_id} in {} target(s)",
        keep.prefix,
        targets.len()
    ))
}

/// The URL written next to the key, as the web UI reaches the API
/// (`workload::api_origin`): `server.publicUrl`, the `Ingress` host, else
/// the in-cluster `Service`.
fn public_url(instance: &api::LocalInstance, endpoint: &str) -> String {
    let spec = &instance.spec;
    match (spec.server.public_origin(), &spec.ingress) {
        (Some(public), _) => public.to_string(),
        (None, Some(ingress)) => format!("https://{}", ingress.api.host),
        (None, None) => endpoint.to_string(),
    }
}

/// A key minted but not written: revoked at once, it is of use to no one
/// (a failed revocation is left to the next pass, which revokes every key
/// of the CR but its own).
async fn unwritten(
    ports: &ApiKeyPorts<'_>,
    endpoint: &str,
    token: &str,
    id: &str,
    e: &SecretStoreError,
) -> Failure {
    let mut failure = secret_failure(e);
    if let Err(revoke) = ports.issuer.revoke(endpoint, token, id).await {
        failure.message = format!("{}; revoking the unwritten key: {revoke}", failure.message);
    }
    failure
}

/// What a deleted key's release is about.
#[derive(Debug, Clone, Copy)]
pub struct ApiKeyRelease<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
    pub spec: &'a ApiKeySpec,
    pub recorded: Option<&'a BoundInstance>,
    /// `notify.weebo.io/force-orphan: "true"`: released at once, the key
    /// and its targets left.
    pub orphaned: bool,
}

/// A deleted key's release (its finalizer): every key of the CR revoked,
/// then its targets deleted. An instance gone or being deleted revokes
/// nothing (nothing will serve the key).
pub async fn release_api_key(
    pass: ApiKeyRelease<'_>,
    ports: ApiKeyPorts<'_>,
) -> Result<(), Failure> {
    if pass.orphaned {
        return Ok(());
    }
    if let Some(bound) = pass.recorded
        && let Some(instance) = ports.instances.instance(bound).await.map_err(api_failure)?
        && instance.metadata.deletion_timestamp.is_none()
        && let Some((endpoint, token)) = issuer_of(&instance, bound, ports.objects).await?
    {
        let label = label(pass.namespace, pass.name);
        let keys = ports
            .issuer
            .list(&endpoint, &token)
            .await
            .map_err(notify_failure)?;
        for key in keys
            .iter()
            .filter(|k| !k.revoked && k.label.as_deref() == Some(label.as_str()))
        {
            ports
                .issuer
                .revoke(&endpoint, &token, &key.id)
                .await
                .map_err(notify_failure)?;
        }
    }
    let owner = owner(pass.namespace, pass.name);
    let targets = emit::targets(&pass.spec.secret_targets, pass.name, ports.secrets);
    emit::delete_all(&targets, &owner, ports.secrets)
        .await
        .map_err(secret_failure)
}

#[cfg(test)]
mod tests;
