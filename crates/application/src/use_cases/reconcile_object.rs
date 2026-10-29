//! A `NotifyChannel`, `NotifyServiceAccount`, `NotifyTransform` or
//! `NotifyTrustPolicy`: bound to its instance (through the instance's
//! consent for another namespace), then what the instance made of it
//! reflected — rendered and confirmed by every pod, or why not
//! (`IdConflict`, `SecretMissing`, `SecretNotAllowed`, `InvalidSpec`,
//! waiting). The instance renders it; this pass writes nothing but the
//! status.

use api::status::{RenderState, object_key};
use domain::error::ReasonCode;
use domain::{Advisory, Failure};

use super::{Outcome, api_failure, instance_failure};
use crate::objects::{ObjectKind, id_of};
use crate::ports::{AttachedInstances, BoundInstance};

/// What a pass is about.
#[derive(Debug, Clone, Copy)]
pub struct ObjectPass<'a> {
    pub kind: ObjectKind,
    pub namespace: &'a str,
    pub name: &'a str,
    pub generation: Option<i64>,
    /// `spec.id` (none for a trust policy).
    pub spec_id: Option<&'a str>,
    pub instance_ref: Option<&'a weebo_kit_api::InstanceRef>,
    pub recorded: Option<&'a BoundInstance>,
}

/// The outcome, and the status fields of an object.
#[derive(Debug, Clone)]
pub struct ObjectReport {
    pub outcome: Outcome,
    /// Its id in Notify.
    pub id: Option<String>,
}

pub async fn reconcile_object(
    pass: ObjectPass<'_>,
    instances: &dyn AttachedInstances,
) -> ObjectReport {
    let id = id_of(pass.kind, pass.name, pass.spec_id);
    let bound = match instances
        .resolve_attached(pass.namespace, pass.instance_ref, pass.recorded)
        .await
    {
        Ok(bound) => bound,
        Err(e) => {
            return ObjectReport {
                outcome: Outcome::failed(instance_failure(e)),
                id,
            };
        }
    };
    let mut advisories = Vec::new();
    let ready = run(&pass, &bound, instances, &mut advisories).await;
    let mut outcome = match ready {
        Ok(message) => Outcome::synced(message),
        Err(failure) => Outcome::failed(failure),
    }
    .on(bound);
    outcome.advisories = advisories;
    ObjectReport { outcome, id }
}

async fn run(
    pass: &ObjectPass<'_>,
    bound: &BoundInstance,
    instances: &dyn AttachedInstances,
    advisories: &mut Vec<Advisory>,
) -> Result<String, Failure> {
    let instance = instances
        .instance(bound)
        .await
        .map_err(api_failure)?
        .ok_or_else(|| Failure::new(ReasonCode::InstanceNotFound, format!("{bound} is gone")))?;
    let status = instance.status.unwrap_or_default();
    let key = object_key(pass.kind.kind(), pass.namespace, pass.name);
    let Some(render) = status.objects.get(&key) else {
        return Err(Failure::new(
            ReasonCode::ConfigPropagating,
            format!("waiting for {bound} to render it"),
        ));
    };
    let message = render.message.clone().unwrap_or_default();
    match render.state {
        RenderState::Conflict => Err(Failure::new(ReasonCode::IdConflict, message)),
        RenderState::SecretMissing => Err(Failure::new(ReasonCode::SecretMissing, message)),
        RenderState::SecretNotAllowed => Err(Failure::new(ReasonCode::SecretNotAllowed, message)),
        RenderState::Invalid => Err(Failure::new(ReasonCode::InvalidSpec, message)),
        RenderState::Rendered => {
            if !render.unmanaged_channels.is_empty() {
                advisories.push(Advisory {
                    reason: ReasonCode::ChannelNotManaged,
                    message: format!(
                        "no NotifyChannel of {bound} declares {}: dynamic channels, or a typo",
                        render.unmanaged_channels.join(", ")
                    ),
                });
            }
            // Confirmed by the instance itself, for this generation: never
            // inferred from its hashes, which move in separate writes.
            // TODO(2c): the instance sets `appliedGeneration` once every
            // pod reports the digest in `/healthz`.
            if render.applied_generation.is_some() && render.applied_generation == pass.generation {
                Ok(format!("applied by every pod of {bound}"))
            } else {
                Err(Failure::new(
                    ReasonCode::ConfigPropagating,
                    format!("rendered into {bound}, waiting for its pods to load it"),
                ))
            }
        }
    }
}

/// What a deleted object's release is about.
#[derive(Debug, Clone, Copy)]
pub struct ReleasePass<'a> {
    pub kind: ObjectKind,
    pub namespace: &'a str,
    pub name: &'a str,
    pub spec_id: Option<&'a str>,
    pub recorded: Option<&'a BoundInstance>,
    /// `notify.weebo.io/force-orphan: "true"`: released at once.
    pub orphaned: bool,
}

/// A deleted object's release (its finalizer): once every pod of its
/// instance applied a file without it. A channel or service account whose
/// removal Notify's guard refused stays (`RemovalRefused`): its CR gone, it
/// would stay managed with nothing to say so. Transforms and trust
/// policies wait too: a trust policy removed must be confirmed revoked.
/// Released at once when the instance is gone or being deleted (nothing
/// will serve it), or the CR is force-orphaned.
pub async fn release_object(
    pass: ReleasePass<'_>,
    instances: &dyn AttachedInstances,
) -> Result<(), Failure> {
    if pass.orphaned {
        return Ok(());
    }
    let Some(bound) = pass.recorded else {
        return Ok(());
    };
    let Some(instance) = instances.instance(bound).await.map_err(api_failure)? else {
        return Ok(());
    };
    if instance.metadata.deletion_timestamp.is_some() {
        return Ok(());
    }
    let status = instance.status.unwrap_or_default();
    let id = id_of(pass.kind, pass.name, pass.spec_id).unwrap_or_default();
    let refused = match pass.kind {
        ObjectKind::Channel => status.refused_removals.channels.contains(&id),
        ObjectKind::ServiceAccount => status.refused_removals.service_accounts.contains(&id),
        ObjectKind::Transform | ObjectKind::TrustPolicy => false,
    };
    if refused {
        return Err(Failure::new(
            ReasonCode::RemovalRefused,
            format!(
                "Notify's guard on {bound} refused removing the {} {id}: more removals at once \
                 than its reconcile.maxRemovals. Set reconcile.allowMassRemoval on the instance \
                 (then unset it), or remove fewer objects at a time",
                pass.kind.noun()
            ),
        ));
    }
    if status
        .objects
        .contains_key(&object_key(pass.kind.kind(), pass.namespace, pass.name))
    {
        return Err(Failure::new(
            ReasonCode::ConfigPropagating,
            format!("waiting for {bound} to render without it"),
        ));
    }
    match (&status.applied_config_digest, &status.config_digest) {
        (Some(applied), Some(written)) if applied == written => Ok(()),
        _ => Err(Failure::new(
            ReasonCode::ConfigPropagating,
            format!("waiting for every pod of {bound} to apply its file without it"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use api::status::{InstanceStatus, ObjectRender};

    use super::*;
    use crate::test_support::FakeInstances;

    fn render(state: RenderState, applied: bool) -> ObjectRender {
        ObjectRender {
            id: Some("alerts".into()),
            state,
            message: Some("why".into()),
            conflict_with: (state == RenderState::Conflict).then(|| "team/older".into()),
            unmanaged_channels: Vec::new(),
            generation: Some(1),
            applied_generation: applied.then_some(1),
        }
    }

    fn instances(render: Option<ObjectRender>) -> FakeInstances {
        let mut status = InstanceStatus::default();
        if let Some(render) = render {
            status
                .objects
                .insert("NotifyChannel/team/alerts".into(), render);
        }
        FakeInstances::with_status(status)
    }

    async fn pass(instances: &FakeInstances) -> ObjectReport {
        reconcile_object(
            ObjectPass {
                kind: ObjectKind::Channel,
                namespace: "team",
                name: "alerts",
                generation: Some(1),
                spec_id: None,
                instance_ref: None,
                recorded: None,
            },
            instances,
        )
        .await
    }

    #[tokio::test]
    async fn a_rendered_object_is_ready_once_its_pods_applied_it() {
        let report = pass(&instances(Some(render(RenderState::Rendered, true)))).await;
        assert_eq!(report.outcome.reason(), ReasonCode::Reconciled);
        assert_eq!(report.id.as_deref(), Some("alerts"));
        assert_eq!(
            report.outcome.instance,
            Some(BoundInstance::local("team", "main"))
        );
        let waiting = pass(&instances(Some(render(RenderState::Rendered, false)))).await;
        assert_eq!(waiting.outcome.reason(), ReasonCode::ConfigPropagating);
        let unseen = pass(&instances(None)).await;
        assert_eq!(unseen.outcome.reason(), ReasonCode::ConfigPropagating);
    }

    #[tokio::test]
    async fn what_the_instance_refused_is_reflected() {
        for (state, reason) in [
            (RenderState::Conflict, ReasonCode::IdConflict),
            (RenderState::SecretMissing, ReasonCode::SecretMissing),
            (RenderState::SecretNotAllowed, ReasonCode::SecretNotAllowed),
            (RenderState::Invalid, ReasonCode::InvalidSpec),
        ] {
            let report = pass(&instances(Some(render(state, true)))).await;
            assert_eq!(report.outcome.reason(), reason, "{state:?}");
        }
        let gone = FakeInstances::default();
        assert_eq!(
            pass(&gone).await.outcome.reason(),
            ReasonCode::InstanceNotFound
        );
    }

    #[tokio::test]
    async fn a_channel_not_managed_is_an_advisory() {
        let mut rendered = render(RenderState::Rendered, true);
        rendered.unmanaged_channels = vec!["dynamic".into()];
        let report = pass(&instances(Some(rendered))).await;
        assert_eq!(report.outcome.reason(), ReasonCode::Reconciled);
        assert_eq!(
            report.outcome.advisories[0].reason,
            ReasonCode::ChannelNotManaged
        );
    }

    fn release<'a>(
        kind: ObjectKind,
        bound: Option<&'a BoundInstance>,
        orphaned: bool,
    ) -> ReleasePass<'a> {
        ReleasePass {
            kind,
            namespace: "team",
            name: "alerts",
            spec_id: None,
            recorded: bound,
            orphaned,
        }
    }

    fn dropped(applied: Option<&str>) -> FakeInstances {
        FakeInstances::with_status(InstanceStatus {
            config_digest: Some("d2".into()),
            applied_config_digest: applied.map(String::from),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn a_deleted_object_waits_for_a_file_without_it_applied() {
        let bound = BoundInstance::local("team", "main");
        let channel = release(ObjectKind::Channel, Some(&bound), false);
        let still = instances(Some(render(RenderState::Rendered, true)));
        let waiting = release_object(channel, &still).await.unwrap_err();
        assert_eq!(waiting.reason, ReasonCode::ConfigPropagating);
        // Out of the file, which the pods haven't all applied yet.
        let written = release_object(channel, &dropped(Some("d1"))).await;
        assert_eq!(written.unwrap_err().reason, ReasonCode::ConfigPropagating);
        assert!(release_object(channel, &dropped(None)).await.is_err());
        // Applied: released.
        assert!(release_object(channel, &dropped(Some("d2"))).await.is_ok());
        // Never bound, or the instance gone.
        let none = release(ObjectKind::Channel, None, false);
        assert!(release_object(none, &still).await.is_ok());
        assert!(
            release_object(channel, &FakeInstances::default())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_refused_removal_keeps_the_finalizer_until_a_later_pass() {
        let bound = BoundInstance::local("team", "main");
        let refused = |channels: &[&str], accounts: &[&str]| {
            FakeInstances::with_status(InstanceStatus {
                config_digest: Some("d2".into()),
                applied_config_digest: Some("d2".into()),
                refused_removals: api::status::RefusedRemovals {
                    channels: channels.iter().map(|c| c.to_string()).collect(),
                    service_accounts: accounts.iter().map(|c| c.to_string()).collect(),
                },
                ..Default::default()
            })
        };
        let channel = release(ObjectKind::Channel, Some(&bound), false);
        let kept = release_object(channel, &refused(&["alerts"], &[]))
            .await
            .unwrap_err();
        assert_eq!(kept.reason, ReasonCode::RemovalRefused);
        assert!(
            kept.message.contains("allowMassRemoval"),
            "{}",
            kept.message
        );
        // The ids are per kind: a service account `alerts` is not this.
        assert!(
            release_object(channel, &refused(&[], &["alerts"]))
                .await
                .is_ok()
        );
        let account = release(ObjectKind::ServiceAccount, Some(&bound), false);
        assert_eq!(
            release_object(account, &refused(&[], &["alerts"]))
                .await
                .unwrap_err()
                .reason,
            ReasonCode::RemovalRefused
        );
        // A later pass the guard allows empties the list: released.
        assert!(release_object(channel, &refused(&[], &[])).await.is_ok());
        // Force-orphaned: released whatever Notify says.
        let orphaned = release(ObjectKind::Channel, Some(&bound), true);
        assert!(
            release_object(orphaned, &refused(&["alerts"], &[]))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn an_instance_being_deleted_releases_its_objects() {
        let bound = BoundInstance::local("team", "main");
        let deleting = FakeInstances::deleting(InstanceStatus::default());
        assert!(
            release_object(
                release(ObjectKind::TrustPolicy, Some(&bound), false),
                &deleting
            )
            .await
            .is_ok()
        );
    }
}
