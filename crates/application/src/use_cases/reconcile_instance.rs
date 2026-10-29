//! A `NotifyLocalInstance`: its configuration and its workload. One pass:
//!
//! 1. the Secrets the spec references are checked (present, with the
//!    credential label): the pods read them, the operator doesn't;
//! 2. the objects bound to it are listed, the Secrets they reference
//!    checked;
//! 3. the file is rendered and checked by Notify's model (`InvalidSpec`:
//!    nothing written), each object's fate recorded in `status.objects`;
//! 4. the image's tag is checked ([`crate::upgrade`],
//!    `UnsupportedVersion`: nothing written);
//! 5. the sinks' secrets are read (the only values the operator reads, to
//!    copy them into `<name>-sinks`), the missing generated keys made;
//! 6. a new tag is migrated first: its `<name>-migrate-<hash8>` `Job` runs,
//!    the server keeps its running image (none yet: no `Deployment`) until
//!    it succeeds (`Migrating`, `MigrationFailed`; then `Migrated`);
//! 7. the objects are applied (none taken over, the pods checked by the
//!    kit), the digest and boot hash recorded at once; the
//!    `ServiceMonitor` apart (no such kind: `MonitoringUnavailable`); what
//!    the spec no longer wants removed, old migration `Job`s pruned;
//! 8. the server's pods all run the target image and boot hash
//!    (`WorkloadNotReady`);
//! 9. the configuration's confirmation from the pods' `/healthz`
//!    ([`crate::confirm`], ADR-0032): `appliedConfigDigest` and each
//!    object's `appliedGeneration` once every ready pod applied the file
//!    (`ConfigPropagating` until then), the refused removals
//!    (`RemovalRefused`), a restart the boot hash missed
//!    (`RestartPending`). `Ready` = migrated, rolled out and applied.

use std::collections::{BTreeMap, BTreeSet};

use api::local_instance::{LocalInstanceSpec, Names};
use api::status::{InstanceStatus, RefusedRemovals, RenderState};
use domain::error::ReasonCode;
use domain::{Advisory, Failure};
use weebo_kit_application::workload::{OperatorPod, stale};

use super::{Outcome, api_failure, credential_failure, workload_failure};
use crate::confirm::{Confirmation, confirm};
use crate::ports::{
    CredentialError, Credentials, Jobs, KeyGenerator, ObjectDirectory, OwnedObjects, Owner,
    ReplicaHealth, StatusLedger, WorkloadError,
};
use crate::render::{self, Input, Rendered, SecretState, SecretStates, paths};
use crate::workload::{self, WorkloadInput};

/// How many migration `Job`s are kept besides the current one (their logs).
pub const KEEP_MIGRATIONS: usize = 1;

/// What a pass is about.
#[derive(Debug, Clone)]
pub struct InstancePass<'a> {
    pub owner: &'a Owner,
    pub spec: &'a LocalInstanceSpec,
    pub annotations: &'a BTreeMap<String, String>,
    /// The status as read (the previous pass's).
    pub status: &'a InstanceStatus,
    /// `repository:tag` of an instance without `image`, server and web.
    pub default_image: &'a str,
    pub default_web_image: &'a str,
    /// The operator's pods, let in by the server's `NetworkPolicy`.
    pub operator: &'a OperatorPod,
}

/// What a pass needs.
pub struct InstancePorts<'a> {
    pub objects: &'a dyn OwnedObjects,
    pub credentials: &'a dyn Credentials,
    pub ledger: &'a dyn StatusLedger,
    pub directory: &'a dyn ObjectDirectory,
    pub jobs: &'a dyn Jobs,
    pub keys: &'a dyn KeyGenerator,
    /// The server pods' `/healthz`.
    pub health: &'a dyn ReplicaHealth,
}

/// A pass's result: the `Ready` outcome, the status to publish, the Events
/// to announce.
#[derive(Debug, Clone)]
pub struct InstanceReport {
    pub outcome: Outcome,
    pub status: InstanceStatus,
    pub events: Vec<(ReasonCode, String)>,
}

/// The status fields a pass writes, for a merge patch against `previous`
/// (`null` clears a field, and an object no longer bound).
#[expect(
    clippy::expect_used,
    reason = "InstanceStatus is a struct of serializable fields: always a JSON object"
)]
pub fn status_fields(
    previous: &InstanceStatus,
    status: &InstanceStatus,
) -> serde_json::Map<String, serde_json::Value> {
    let mut value = serde_json::to_value(status).expect("a status serializes");
    let map = value.as_object_mut().expect("a status is an object");
    map.remove("conditions");
    map.remove("observedGeneration");
    for key in [
        "endpoint",
        "version",
        "migratedVersion",
        "availableReplicas",
        "bootHash",
        "configDigest",
        "appliedConfigDigest",
        "refusedRemovals",
    ] {
        map.entry(key).or_insert(serde_json::Value::Null);
    }
    // A merge patch merges maps: what left `objects` is dropped by name.
    let objects = map
        .entry("objects")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(objects) = objects.as_object_mut() {
        for key in previous.objects.keys() {
            objects
                .entry(key.clone())
                .or_insert(serde_json::Value::Null);
        }
    }
    map.clone()
}

pub async fn reconcile_instance(
    pass: InstancePass<'_>,
    ports: &InstancePorts<'_>,
) -> InstanceReport {
    let mut status = pass.status.clone();
    let mut advisories = Vec::new();
    let mut events = Vec::new();
    let ready = run(&pass, ports, &mut status, &mut advisories, &mut events).await;
    let mut outcome = match ready {
        Ok(message) => Outcome::synced(message),
        Err(failure) => Outcome::failed(failure),
    };
    if let Some(version) = &status.migrated_version {
        advisories.push(Advisory {
            reason: ReasonCode::Migrated,
            message: format!("the database is migrated to Notify {version}"),
        });
    }
    outcome.advisories = advisories;
    InstanceReport {
        outcome,
        status,
        events,
    }
}

/// Where the migration to the target image stands.
enum Migration {
    Done,
    Running,
    Failed(String),
}

/// The `Secret`s render names that the operator generates, and whether
/// each is an Ed25519 key (else a random string).
fn generated_secrets(rendered: &Rendered) -> BTreeMap<String, bool> {
    let mut out: BTreeMap<String, bool> = rendered
        .env
        .iter()
        .filter(|e| e.generated)
        .map(|e| (e.source.name.clone(), false))
        .collect();
    for file in rendered.files.iter().filter(|f| f.generated) {
        out.insert(
            file.source.name.clone(),
            file.path == paths::FEDERATION_KEY_FILE,
        );
    }
    out
}

/// The 404 of a kind the cluster doesn't serve, as the kit's adapter
/// words a failed apply.
fn kind_missing(e: &WorkloadError) -> bool {
    matches!(e, WorkloadError::Kubernetes(m) if m.contains("404"))
}

async fn check_secrets(pass: &InstancePass<'_>, ports: &InstancePorts<'_>) -> Result<(), Failure> {
    for source in render::instance_secrets(&pass.owner.namespace, pass.spec) {
        ports
            .credentials
            .check(&source.namespace, &source.name, &source.key)
            .await
            .map_err(credential_failure)?;
    }
    Ok(())
}

async fn render_file(
    pass: &InstancePass<'_>,
    ports: &InstancePorts<'_>,
) -> Result<Rendered, Failure> {
    let (owner, spec) = (pass.owner, pass.spec);
    let views = ports
        .directory
        .bound_to(owner, &spec.object_selector)
        .await
        .map_err(api_failure)?;
    let mut secrets = SecretStates::new();
    for source in render::object_secrets(&views) {
        let state = match ports
            .credentials
            .check(&source.namespace, &source.name, &source.key)
            .await
        {
            Ok(()) => SecretState::Present,
            Err(CredentialError::Missing(m)) => SecretState::Missing(m),
            Err(CredentialError::NotAllowed(m)) => SecretState::NotAllowed(m),
            Err(e @ CredentialError::Kubernetes(_)) => return Err(credential_failure(e)),
        };
        secrets.insert(source, state);
    }
    render::render(&Input {
        namespace: &owner.namespace,
        name: &owner.name,
        spec,
        objects: &views,
        secrets: &secrets,
        previous: &pass.status.objects,
    })
    .map_err(|e| Failure::new(ReasonCode::InvalidSpec, e.to_string()))
}

/// The sinks' secrets, by their file name, and the generated keys not
/// made yet.
async fn values(
    pass: &InstancePass<'_>,
    ports: &InstancePorts<'_>,
    rendered: &Rendered,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>), Failure> {
    let mut sinks = BTreeMap::new();
    for file in &rendered.files {
        let Some(entry) = file
            .path
            .strip_prefix(paths::SINK_SECRETS_DIR)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            continue;
        };
        let value = ports
            .credentials
            .read(&file.source.namespace, &file.source.name, &file.source.key)
            .await
            .map_err(credential_failure)?;
        sinks.insert(entry.to_string(), value);
    }
    let mut generated = BTreeMap::new();
    for (name, ed25519) in generated_secrets(rendered) {
        let existing = ports
            .objects
            .secret_text(pass.owner, &name, paths::GENERATED_KEY)
            .await
            .map_err(workload_failure)?;
        if existing.is_none() {
            let key = if ed25519 {
                ports.keys.ed25519_pem()
            } else {
                ports.keys.key()
            };
            generated.insert(name, key);
        }
    }
    Ok((sinks, generated))
}

async fn run(
    pass: &InstancePass<'_>,
    ports: &InstancePorts<'_>,
    status: &mut InstanceStatus,
    advisories: &mut Vec<Advisory>,
    events: &mut Vec<(ReasonCode, String)>,
) -> Result<String, Failure> {
    let (owner, spec) = (pass.owner, pass.spec);
    let names = Names(&owner.name);
    status.endpoint = Some(format!(
        "http://{}.{}.svc:{}",
        names.server(),
        owner.namespace,
        paths::PORT
    ));

    // 1–3. The Secrets, the objects, the file.
    check_secrets(pass, ports).await?;
    let rendered = render_file(pass, ports).await?;
    status.objects = rendered.objects.clone();

    // 4. The version.
    let target = workload::image(&spec.image, pass.default_image);
    let target_tag = workload::tag(&target).to_string();
    crate::upgrade::check(
        status.migrated_version.as_deref(),
        &target_tag,
        pass.annotations
            .get(api::annotations::APPROVED_UPGRADE)
            .map(String::as_str),
    )
    .map_err(|e| Failure::new(ReasonCode::UnsupportedVersion, e.to_string()))?;

    // 5. The values the objects carry.
    let (sinks, generated) = values(pass, ports, &rendered).await?;

    // 6. Migrated first.
    let running = ports
        .objects
        .rollout(owner, &names.server())
        .await
        .map_err(workload_failure)?;
    let running_image = running
        .as_ref()
        .and_then(|r| r.annotation(workload::IMAGE_ANNOTATION))
        .map(String::from);
    let job = workload::migrate_job(&owner.name, &target);
    let migration = if status.migrated_version.as_deref() == Some(target_tag.as_str()) {
        Migration::Done
    } else {
        match ports
            .jobs
            .job(&owner.namespace, &job)
            .await
            .map_err(api_failure)?
        {
            Some(state) if state.succeeded => {
                status.migrated_version = Some(target_tag.clone());
                events.push((
                    ReasonCode::Migrated,
                    format!("the database was migrated to Notify {target_tag} ({job})"),
                ));
                Migration::Done
            }
            Some(state) => match state.failed {
                Some(why) => Migration::Failed(why),
                None => Migration::Running,
            },
            None => Migration::Running,
        }
    };
    let (server_image, migrating) = match migration {
        Migration::Done => (Some(target.as_str()), None),
        _ => (running_image.as_deref(), Some(target.as_str())),
    };

    // 7. The objects.
    let grow_claim = match &spec.attachments.filesystem {
        Some(_) => workload::claim_grows(
            spec,
            ports
                .objects
                .claim_size(owner, &names.attachments())
                .await
                .map_err(workload_failure)?
                .as_deref(),
        ),
        None => false,
    };
    let web_image = workload::image(&spec.web.image, pass.default_web_image);
    let input = WorkloadInput {
        name: &owner.name,
        namespace: &owner.namespace,
        spec,
        rendered: &rendered,
        server_image,
        migration: migrating,
        web_image: &web_image,
        generated: &generated,
        sinks: &sinks,
        grow_claim,
        operator: pass.operator,
    };
    ports
        .objects
        .apply(owner, &workload::objects(&input))
        .await
        .map_err(workload_failure)?;
    if spec.web.enabled && !workload::api_origin(&input).1 {
        advisories.push(Advisory {
            reason: ReasonCode::ApiNotExposed,
            message: "the web UI is on but nothing exposes the API to browsers: set \
                      server.publicUrl (a Gateway, a proxy) or ingress"
                .into(),
        });
    }
    status.config_digest = Some(rendered.config_digest.clone());
    status.boot_hash = Some(rendered.boot_hash.clone());
    ports
        .ledger
        .record(
            &owner.namespace,
            &owner.name,
            status_fields(pass.status, status),
        )
        .await
        .map_err(api_failure)?;
    if let Some(monitor) = workload::service_monitor(&input) {
        match ports.objects.apply(owner, &[monitor]).await {
            Ok(()) => {}
            Err(e) if kind_missing(&e) => advisories.push(Advisory {
                reason: ReasonCode::MonitoringUnavailable,
                message: "the cluster has no ServiceMonitor kind (monitoring.coreos.com/v1): \
                          none is created"
                    .into(),
            }),
            Err(e) => return Err(workload_failure(e)),
        }
    }
    for (api_version, kind, name) in workload::removed(&input) {
        ports
            .objects
            .remove(owner, api_version, kind, &name)
            .await
            .map_err(workload_failure)?;
    }
    let jobs = ports
        .objects
        .list(
            owner,
            "batch/v1",
            "Job",
            &workload::selector(&owner.name, "migrate"),
        )
        .await
        .map_err(workload_failure)?;
    for old in stale(&jobs, &BTreeSet::from([job.clone()]), KEEP_MIGRATIONS) {
        ports
            .objects
            .remove(owner, "batch/v1", "Job", &old)
            .await
            .map_err(workload_failure)?;
    }

    // 8. Where the pods stand.
    let rollout = ports
        .objects
        .rollout(owner, &names.server())
        .await
        .map_err(workload_failure)?;
    status.available_replicas = rollout.as_ref().map(|r| r.available);
    let on_target = rollout.as_ref().is_some_and(|r| {
        r.complete()
            && r.annotation(workload::IMAGE_ANNOTATION) == Some(target.as_str())
            && r.annotation(api::annotations::BOOT_HASH) == Some(rendered.boot_hash.as_str())
    });
    if let Some(image) = rollout
        .as_ref()
        .filter(|r| r.complete())
        .and_then(|r| r.annotation(workload::IMAGE_ANNOTATION))
    {
        status.version = Some(workload::tag(image).to_string());
    }

    // 9. What the pods applied: the file, each object's generation in it,
    // the removals Notify's guard refused.
    let mut waiting = None;
    if rollout.is_some() {
        let replicas = ports
            .health
            .replicas(&owner.namespace, &workload::selector(&owner.name, "server"))
            .await
            .map_err(api_failure)?;
        let pods = confirm(
            &rendered.config_digest,
            &replicas,
            status.available_replicas.unwrap_or_default(),
        );
        apply_confirmation(&rendered, &pods, on_target, status, advisories, events);
        waiting = pods.waiting;
    }
    match migration {
        Migration::Failed(why) => Err(Failure::new(
            ReasonCode::MigrationFailed,
            format!(
                "the migration Job {job} to Notify {target_tag} failed ({why}): the pods stay on \
                 {}; kubectl logs job/{job}",
                running_image.as_deref().unwrap_or("nothing")
            ),
        )),
        Migration::Running => Err(Failure::new(
            ReasonCode::Migrating,
            format!("the database is being migrated to Notify {target_tag} ({job})"),
        )),
        Migration::Done if on_target && waiting.is_none() => Ok(format!(
            "Notify {target_tag} runs the configuration on {} pod(s)",
            status.available_replicas.unwrap_or_default()
        )),
        Migration::Done if on_target => Err(Failure::new(
            ReasonCode::ConfigPropagating,
            format!(
                "configuration written to {}, waiting for the pods to load it: {}",
                names.config(),
                waiting.unwrap_or_default()
            ),
        )),
        Migration::Done => Err(Failure::new(
            ReasonCode::WorkloadNotReady,
            format!(
                "the server's pods are rolling out Notify {target_tag} ({}/{} available)",
                status.available_replicas.unwrap_or_default(),
                spec.replicas
            ),
        )),
    }
}

/// A deleted instance's release (its finalizer): it waits while live
/// objects are still bound to it — its owned objects then go with it
/// (owner references). Force-orphaned: released at once.
pub async fn release_instance(
    owner: &Owner,
    spec: &LocalInstanceSpec,
    orphaned: bool,
    directory: &dyn ObjectDirectory,
) -> Result<(), Failure> {
    if orphaned {
        return Ok(());
    }
    let bound: Vec<String> = directory
        .bound_to(owner, &spec.object_selector)
        .await
        .map_err(api_failure)?
        .into_iter()
        .filter(|v| !v.deleting)
        .map(|v| v.key())
        .collect();
    if bound.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = bound.iter().take(5).map(String::as_str).collect();
    let more = bound.len().saturating_sub(shown.len());
    Err(Failure::new(
        ReasonCode::InstanceInUse,
        format!(
            "{} object(s) still bound to it: {}{}. Delete them (or annotate the instance {}: \
             \"true\" to leave them)",
            bound.len(),
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            },
            api::ANNOTATIONS.key(weebo_kit_api::annotations::FORCE_ORPHAN)
        ),
    ))
}

/// What the pods said, into the status: the applied digest and the
/// generations it holds once every pod runs the file (a `ConfigApplied`
/// Event once per digest), the refused removals, what only a restart
/// applies although the boot configuration is current.
fn apply_confirmation(
    rendered: &Rendered,
    pods: &Confirmation,
    on_target: bool,
    status: &mut InstanceStatus,
    advisories: &mut Vec<Advisory>,
    events: &mut Vec<(ReasonCode, String)>,
) {
    status.refused_removals = RefusedRemovals {
        channels: pods.refused_channels.clone(),
        service_accounts: pods.refused_service_accounts.clone(),
    };
    if !status.refused_removals.is_empty() {
        let ids: Vec<String> = pods
            .refused_channels
            .iter()
            .map(|id| format!("channel {id}"))
            .chain(
                pods.refused_service_accounts
                    .iter()
                    .map(|id| format!("service account {id}")),
            )
            .collect();
        advisories.push(Advisory {
            reason: ReasonCode::RemovalRefused,
            message: format!(
                "Notify's guard refused removing {} (more than reconcile.maxRemovals at once): \
                 they stay managed until reconcile.allowMassRemoval is set or fewer are removed",
                ids.join(", ")
            ),
        });
    }
    if on_target && !pods.pending_restart.is_empty() {
        advisories.push(Advisory {
            reason: ReasonCode::RestartPending,
            message: format!(
                "the pods run the current boot configuration but report what only a restart \
                 applies: {} (restart them: kubectl rollout restart)",
                pods.pending_restart.join("; ")
            ),
        });
    }
    if pods.applied {
        if status.applied_config_digest.as_deref() != Some(rendered.config_digest.as_str()) {
            events.push((
                ReasonCode::ConfigApplied,
                format!(
                    "every pod applied the configuration {}",
                    rendered.config_digest.get(..12).unwrap_or_default()
                ),
            ));
        }
        status.applied_config_digest = Some(rendered.config_digest.clone());
        for render in status.objects.values_mut() {
            if render.state == RenderState::Rendered {
                render.applied_generation = render.generation;
            }
        }
    }
}

#[cfg(test)]
mod tests;
