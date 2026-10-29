//! What the admission webhook refuses before a CR exists, and what it
//! only warns about. The reconcilers check the same again (a Secret, an
//! instance may change after admission).
//!
//! - A `NotifyLocalInstance` is rendered alone, through the reconcile's
//!   own render (Notify's model, `notify-config`): what Notify would
//!   refuse is refused, with Notify's message. So are pods the operator
//!   doesn't run (an image off the allow-list…) and a tag the version
//!   guard refuses. A Secret it names that exists without the credential
//!   label is refused (as weebo-angos does); one that doesn't exist yet is
//!   a warning — it may be created right after.
//! - A channel, service account, transform or trust policy is rendered
//!   into its instance's file with the objects already bound to it: what
//!   Notify refuses of it is refused (a transform's CEL that doesn't
//!   compile, a feed whose publisher may not publish…). Without an
//!   instance yet, it is checked against a placeholder one (a warning
//!   says so). An id an older object holds, a Secret missing: warnings
//!   (the status says `IdConflict`, `SecretMissing`).

use std::collections::BTreeMap;

use api::LocalInstance;
use api::local_instance::LocalInstanceSpec;
use api::status::RenderState;
use domain::Failure;
use domain::error::ReasonCode;
use weebo_kit_application::pod_safety;
use weebo_kit_application::workload::OperatorPod;

use crate::objects::{ObjectSpec, ObjectView};
use crate::ports::{
    AttachedInstances, BoundInstance, CredentialError, Credentials, InstanceError, ObjectDirectory,
    Owner, ResolveError, instance_reason,
};
use crate::render::{self, Input, SecretState, SecretStates};
use crate::workload::{self, WorkloadInput};

/// What an admitted CR is told: warnings the API server shows `kubectl`.
pub type Warnings = Vec<String>;

/// The operator's settings admission needs.
#[derive(Debug, Clone)]
pub struct Settings<'a> {
    pub allowed_images: &'a [String],
    pub default_image: &'a str,
    pub default_web_image: &'a str,
}

/// A Secret of the spec: refused when it doesn't consent, a warning when
/// it doesn't exist (yet).
async fn secret(
    credentials: &dyn Credentials,
    source: &render::SecretSource,
    warnings: &mut Warnings,
) -> Result<SecretState, Failure> {
    match credentials
        .check(&source.namespace, &source.name, &source.key)
        .await
    {
        Ok(()) => Ok(SecretState::Present),
        Err(CredentialError::Missing(m)) => {
            warnings.push(format!("{m}: the operator waits for it"));
            Ok(SecretState::Missing(m))
        }
        Err(e) => Err(Failure::new(e.reason(), e.to_string())),
    }
}

/// An instance created or updated: `status` is the one it carries (on an
/// update), for the version guard.
pub async fn check_instance(
    name: &str,
    namespace: &str,
    instance: &LocalInstance,
    settings: &Settings<'_>,
    credentials: &dyn Credentials,
) -> Result<Warnings, Failure> {
    let spec = &instance.spec;
    let rendered = render::render(&Input {
        namespace,
        name,
        spec,
        objects: &[],
        secrets: &SecretStates::new(),
        previous: &BTreeMap::new(),
    })
    .map_err(|e| Failure::new(ReasonCode::InvalidSpec, e.to_string()))?;

    let target = workload::image(&spec.image, settings.default_image);
    let migrated = instance
        .status
        .as_ref()
        .and_then(|s| s.migrated_version.as_deref());
    let approved = instance
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(api::annotations::APPROVED_UPGRADE))
        .map(String::as_str);
    crate::upgrade::check(migrated, workload::tag(&target), approved)
        .map_err(|e| Failure::new(ReasonCode::UnsupportedVersion, e.to_string()))?;

    let web_image = workload::image(&spec.web.image, settings.default_web_image);
    let none = BTreeMap::new();
    let operator = OperatorPod::named("weebo-notify");
    let objects = workload::objects(&WorkloadInput {
        name,
        namespace,
        spec,
        rendered: &rendered,
        server_image: Some(&target),
        migration: Some(&target),
        web_image: &web_image,
        generated: &none,
        sinks: &none,
        grow_claim: false,
        operator: &operator,
    });
    let problems = pod_safety::problems(&objects, settings.allowed_images);
    if !problems.is_empty() {
        return Err(Failure::new(
            ReasonCode::InvalidSpec,
            format!(
                "the instance asks for pods the operator doesn't run: {}. An instance runs \
                 Notify's images (the operator's allow-list: {})",
                pod_safety::summary(&problems),
                settings.allowed_images.join(", ")
            ),
        ));
    }

    let mut warnings = Warnings::new();
    for source in render::instance_secrets(namespace, spec) {
        secret(credentials, &source, &mut warnings).await?;
    }
    Ok(warnings)
}

/// What an object's admission reads.
pub struct ObjectPorts<'a> {
    pub attached: &'a dyn AttachedInstances,
    pub directory: &'a dyn ObjectDirectory,
    pub credentials: &'a dyn Credentials,
}

/// An object created or updated, as a view, with its `instanceRef` and
/// the instance its status records.
pub struct ObjectReview<'a> {
    pub view: ObjectView,
    pub instance_ref: Option<&'a weebo_kit_api::InstanceRef>,
    pub recorded: Option<&'a BoundInstance>,
}

/// An instance spec that renders, standing in for one not created yet:
/// it has an SMTP relay (an email sink is checked as far as it can be).
fn placeholder() -> Result<LocalInstanceSpec, Failure> {
    serde_json::from_value(serde_json::json!({
        "database": {"urlSecretRef": {"name": "placeholder", "key": "url"}},
        "attachments": {"s3": {"endpoint": "https://s3.placeholder", "bucket": "placeholder",
                               "credentialsSecretRef": {"name": "placeholder"}}},
        "oidc": {"issuer": "https://issuer.placeholder", "clientId": "placeholder"},
        "smtp": {"url": "smtps://smtp.placeholder:465", "from": "notify@placeholder.invalid"},
    }))
    .map_err(|e| {
        Failure::new(
            ReasonCode::InvalidSpec,
            format!("placeholder instance: {e}"),
        )
    })
}

/// The service accounts a channel's feeds name, as placeholders (no
/// instance to find them on).
fn feed_publishers(view: &ObjectView) -> Vec<ObjectView> {
    let ObjectSpec::Channel(channel) = &view.spec else {
        return Vec::new();
    };
    channel
        .feeds
        .iter()
        .map(|feed| ObjectView {
            namespace: view.namespace.clone(),
            name: format!("placeholder-{}", feed.publisher),
            created: None,
            generation: None,
            spec: ObjectSpec::ServiceAccount(api::service_account::ServiceAccountSpec {
                id: Some(feed.publisher.clone()),
                ..Default::default()
            }),
            deleting: false,
        })
        .collect()
}

pub async fn check_object(
    review: ObjectReview<'_>,
    ports: &ObjectPorts<'_>,
) -> Result<Warnings, Failure> {
    let mut warnings = Warnings::new();
    let view = review.view;
    // Not resolvable yet (no instance): checked against a placeholder, the
    // reconcile says why it waits. Any other failure is refused, as the
    // reconcile would.
    let instance = match ports
        .attached
        .resolve_attached(&view.namespace, review.instance_ref, review.recorded)
        .await
    {
        Ok(bound) => ports
            .attached
            .instance(&bound)
            .await
            .map_err(|e| Failure::new(e.reason(), e.to_string()))?,
        Err(InstanceError::Resolve(ResolveError::NoInstance | ResolveError::NotFound { .. })) => {
            None
        }
        Err(e) => return Err(Failure::new(instance_reason(&e), e.to_string())),
    };
    let key = view.key();
    let (name, namespace, spec, mut others) = match &instance {
        Some(instance) => {
            let owner = Owner {
                namespace: instance.metadata.namespace.clone().unwrap_or_default(),
                name: instance.metadata.name.clone().unwrap_or_default(),
                uid: instance.metadata.uid.clone().unwrap_or_default(),
            };
            let bound = ports
                .directory
                .bound_to(&owner, &instance.spec.object_selector)
                .await
                .map_err(|e| Failure::new(e.reason(), e.to_string()))?;
            (owner.name, owner.namespace, instance.spec.clone(), bound)
        }
        None => {
            warnings.push(
                "no instance serves it yet: checked against a placeholder instance, the \
                 operator binds it once one exists"
                    .to_string(),
            );
            let publishers = feed_publishers(&view);
            (
                "placeholder".into(),
                view.namespace.clone(),
                placeholder()?,
                publishers,
            )
        }
    };
    // This object as created or updated, not as it was.
    others.retain(|o| o.key() != key);
    let mut secrets = SecretStates::new();
    for source in render::object_secrets(std::slice::from_ref(&view)) {
        let state = secret(ports.credentials, &source, &mut warnings).await?;
        secrets.insert(source, state);
    }
    others.push(view);
    let rendered = match render::render(&Input {
        namespace: &namespace,
        name: &name,
        spec: &spec,
        objects: &others,
        secrets: &secrets,
        previous: &BTreeMap::new(),
    }) {
        Ok(rendered) => rendered,
        // The instance's own spec doesn't render: its status says why.
        Err(e) => {
            warnings.push(format!(
                "its instance {namespace}/{name} doesn't render: {e}"
            ));
            return Ok(warnings);
        }
    };
    let Some(render) = rendered.objects.get(&key) else {
        return Ok(warnings);
    };
    let message = render.message.clone().unwrap_or_default();
    match render.state {
        RenderState::Rendered => {
            if !render.unmanaged_channels.is_empty() {
                warnings.push(format!(
                    "no NotifyChannel of {namespace}/{name} declares {}: dynamic channels, or a \
                     typo",
                    render.unmanaged_channels.join(", ")
                ));
            }
            Ok(warnings)
        }
        RenderState::Invalid => Err(Failure::new(ReasonCode::InvalidSpec, message)),
        RenderState::Conflict => {
            warnings.push(message);
            Ok(warnings)
        }
        // Already said by the Secret's own check.
        RenderState::SecretMissing => Ok(warnings),
        RenderState::SecretNotAllowed => Err(Failure::new(ReasonCode::SecretNotAllowed, message)),
    }
}

#[cfg(test)]
mod tests;
