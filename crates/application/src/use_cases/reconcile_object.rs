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

/// A deleted object's release (its finalizer): it waits until its instance
/// re-rendered without it.
///
/// TODO(2c): until every pod applied that file, and — when Notify's guard
/// refused the removal (`status.refusedRemovals`) — `RemovalRefused`.
pub async fn release_object(
    kind: ObjectKind,
    namespace: &str,
    name: &str,
    recorded: Option<&BoundInstance>,
    instances: &dyn AttachedInstances,
) -> Result<(), Failure> {
    let Some(bound) = recorded else {
        return Ok(());
    };
    let Some(instance) = instances.instance(bound).await.map_err(api_failure)? else {
        return Ok(());
    };
    let status = instance.status.unwrap_or_default();
    if status
        .objects
        .contains_key(&object_key(kind.kind(), namespace, name))
    {
        Err(Failure::new(
            ReasonCode::ConfigPropagating,
            format!("waiting for {bound} to render without it"),
        ))
    } else {
        Ok(())
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

    #[tokio::test]
    async fn a_deleted_object_waits_for_its_instance_to_drop_it() {
        let bound = BoundInstance::local("team", "main");
        let still = instances(Some(render(RenderState::Rendered, true)));
        let waiting = release_object(ObjectKind::Channel, "team", "alerts", Some(&bound), &still)
            .await
            .unwrap_err();
        assert_eq!(waiting.reason, ReasonCode::ConfigPropagating);
        let dropped = instances(None);
        assert!(
            release_object(
                ObjectKind::Channel,
                "team",
                "alerts",
                Some(&bound),
                &dropped
            )
            .await
            .is_ok()
        );
        assert!(
            release_object(ObjectKind::Channel, "team", "alerts", None, &still)
                .await
                .is_ok()
        );
    }
}
