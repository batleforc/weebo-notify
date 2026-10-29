//! The validating webhook (`POST /validate`): the five kinds checked on
//! `CREATE`/`UPDATE` by `application::admission` — what Notify's own model
//! refuses is refused with Notify's message; what may resolve itself (a
//! Secret not created yet, no instance yet, an id an older object holds)
//! is admitted with a warning `kubectl` shows.
//!
//! Served by the kit's `admission::router` ([`crate::webhook`]): a
//! `DELETE` is always allowed (a cleanup is never stranded), an `UPDATE`
//! leaving `spec` alone is allowed (finalizer removal, labels), anything
//! not understood fails closed, a refusal is a 403 with its reason code,
//! the warnings go in the review's `warnings`, and every decision counts
//! in `<prefix>_webhook_decisions_total`.

use std::sync::Arc;

use application::admission::{
    ObjectPorts, ObjectReview, Settings, Warnings, check_instance, check_object,
};
use application::objects::{ObjectSpec, ObjectView};
use application::ports::{AttachedInstances, Credentials, ObjectDirectory};
use domain::Failure;
use domain::error::ReasonCode;
use kube::ResourceExt;
use kube::core::DynamicObject;
use weebo_kit_api::{BoundInstance, InstanceRef};
use weebo_kit_runtime::admission::{Admission, Denial, typed};

pub struct NotifyAdmission {
    pub credentials: Arc<dyn Credentials>,
    pub attached: Arc<dyn AttachedInstances>,
    pub directory: Arc<dyn ObjectDirectory>,
    pub allowed_images: Vec<String>,
    pub default_image: String,
    pub default_web_image: String,
}

fn invalid(e: impl std::fmt::Display) -> Failure {
    Failure::new(ReasonCode::InvalidSpec, e.to_string())
}

/// An object CR as the render sees it, with its `instanceRef` and the
/// instance its status records.
fn object_view<K, S>(
    object: &DynamicObject,
    spec: impl Fn(&K) -> (S, Option<InstanceRef>),
    wrap: impl Fn(S) -> ObjectSpec,
    status: impl Fn(&K) -> Option<BoundInstance>,
) -> Result<(ObjectView, Option<InstanceRef>, Option<BoundInstance>), Failure>
where
    K: serde::de::DeserializeOwned,
{
    let typed_object: K = typed(object).map_err(invalid)?;
    let (spec, instance_ref) = spec(&typed_object);
    let view = ObjectView {
        namespace: object.namespace().unwrap_or_default(),
        name: object.name_any(),
        created: object.metadata.creation_timestamp.as_ref().map(|t| t.0),
        generation: object.metadata.generation,
        spec: wrap(spec),
        deleting: false,
    };
    Ok((view, instance_ref, status(&typed_object)))
}

impl NotifyAdmission {
    /// The decision on `object` of kind `kind`, created or updated in
    /// `namespace`: `Ok(warnings)` admits it.
    pub async fn decide(
        &self,
        kind: &str,
        namespace: &str,
        object: &DynamicObject,
    ) -> Result<Warnings, Failure> {
        let (view, instance_ref, recorded) = match kind {
            api::kinds::CHANNEL => object_view(
                object,
                |c: &api::Channel| (c.spec.clone(), c.spec.instance_ref.clone()),
                ObjectSpec::Channel,
                |c| c.status.as_ref()?.instance.clone(),
            )?,
            api::kinds::SERVICE_ACCOUNT => object_view(
                object,
                |s: &api::ServiceAccount| (s.spec.clone(), s.spec.instance_ref.clone()),
                ObjectSpec::ServiceAccount,
                |s| s.status.as_ref()?.instance.clone(),
            )?,
            api::kinds::TRANSFORM => object_view(
                object,
                |t: &api::Transform| (t.spec.clone(), t.spec.instance_ref.clone()),
                ObjectSpec::Transform,
                |t| t.status.as_ref()?.instance.clone(),
            )?,
            api::kinds::TRUST_POLICY => object_view(
                object,
                |t: &api::TrustPolicy| (t.spec.clone(), t.spec.instance_ref.clone()),
                ObjectSpec::TrustPolicy,
                |t| t.status.as_ref()?.instance.clone(),
            )?,
            "NotifyLocalInstance" => {
                let instance: api::LocalInstance = typed(object).map_err(invalid)?;
                return check_instance(
                    &object.name_any(),
                    namespace,
                    &instance,
                    &Settings {
                        allowed_images: &self.allowed_images,
                        default_image: &self.default_image,
                        default_web_image: &self.default_web_image,
                    },
                    self.credentials.as_ref(),
                )
                .await;
            }
            other => {
                return Err(invalid(format!(
                    "kind {other} is not one the operator admits"
                )));
            }
        };
        check_object(
            ObjectReview {
                view,
                instance_ref: instance_ref.as_ref(),
                recorded: recorded.as_ref(),
            },
            &ObjectPorts {
                attached: self.attached.as_ref(),
                directory: self.directory.as_ref(),
                credentials: self.credentials.as_ref(),
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl Admission for NotifyAdmission {
    type Reason = ReasonCode;

    async fn admit(
        &self,
        kind: &str,
        namespace: &str,
        object: &DynamicObject,
    ) -> Result<(), Denial<ReasonCode>> {
        self.admit_with_warnings(kind, namespace, object)
            .await
            .map(drop)
    }

    async fn admit_with_warnings(
        &self,
        kind: &str,
        namespace: &str,
        object: &DynamicObject,
    ) -> Result<Vec<String>, Denial<ReasonCode>> {
        self.decide(kind, namespace, object)
            .await
            .map_err(|f| Denial::new(f.reason, f.message))
    }

    fn undecidable(&self) -> ReasonCode {
        ReasonCode::InvalidSpec
    }
}

#[cfg(test)]
mod tests {
    use application::test_support::{FakeCredentials, FakeDirectory, FakeInstances};
    use kube::core::admission::AdmissionReview;
    use serde_json::{Value, json};
    use weebo_kit_runtime::admission::handle;

    use super::*;

    fn admission() -> NotifyAdmission {
        let credentials = FakeCredentials::default();
        credentials.insert("team", "db", &[("url", "postgres://u:p@db/notify")]);
        NotifyAdmission {
            credentials: Arc::new(credentials),
            attached: Arc::new(FakeInstances::with_status(Default::default())),
            directory: Arc::new(FakeDirectory::default()),
            allowed_images: vec![
                api::notify::SERVER_IMAGE.into(),
                api::notify::WEB_IMAGE.into(),
            ],
            default_image: format!("{}:{}", api::notify::SERVER_IMAGE, api::notify::VERSION),
            default_web_image: format!("{}:{}", api::notify::WEB_IMAGE, api::notify::VERSION),
        }
    }

    fn review(
        operation: &str,
        kind: &str,
        object: Value,
        old: Option<Value>,
    ) -> AdmissionReview<DynamicObject> {
        serde_json::from_value(json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "uid-1",
                "kind": {"group": "notify.weebo.io", "version": "v1alpha1", "kind": kind},
                "resource": {"group": "notify.weebo.io", "version": "v1alpha1", "resource": "x"},
                "name": "main",
                "namespace": "team",
                "operation": operation,
                "userInfo": {},
                "object": object,
                "oldObject": old,
                "dryRun": false,
            }
        }))
        .unwrap()
    }

    fn instance(spec: Value) -> Value {
        json!({
            "apiVersion": "notify.weebo.io/v1alpha1", "kind": "NotifyLocalInstance",
            "metadata": {"name": "main", "namespace": "team"}, "spec": spec,
        })
    }

    fn spec() -> Value {
        json!({
            "database": {"urlSecretRef": {"name": "db", "key": "url"}},
            "attachments": {"s3": {"endpoint": "http://rustfs:9000", "bucket": "notify",
                                   "credentialsSecretRef": {"name": "s3"}}},
            "oidc": {"issuer": "https://id.example", "clientId": "notify"},
        })
    }

    fn answer(review: &AdmissionReview<DynamicObject>) -> (bool, Value) {
        let response = review.response.as_ref().unwrap();
        (response.allowed, serde_json::to_value(response).unwrap())
    }

    #[tokio::test]
    async fn an_admitted_instance_carries_its_warnings() {
        let reviewed = handle(
            &admission(),
            review("CREATE", "NotifyLocalInstance", instance(spec()), None),
        )
        .await;
        let (allowed, response) = answer(&reviewed);
        assert!(allowed);
        assert_eq!(response["uid"], "uid-1");
        // The S3 Secret doesn't exist yet.
        let warnings = response["warnings"].as_array().unwrap();
        assert!(
            warnings.iter().any(|w| w.as_str().unwrap().contains("s3")),
            "{warnings:?}"
        );
    }

    #[tokio::test]
    async fn a_refusal_carries_its_reason_code() {
        let mut bad = spec();
        bad["image"] = json!({"tag": "9.9.9"});
        let reviewed = handle(
            &admission(),
            review("CREATE", "NotifyLocalInstance", instance(bad), None),
        )
        .await;
        let (allowed, response) = answer(&reviewed);
        assert!(!allowed);
        assert_eq!(response["status"]["code"], 403);
        assert_eq!(response["status"]["reason"], "UnsupportedVersion");
        let transform = json!({
            "apiVersion": "notify.weebo.io/v1alpha1", "kind": "NotifyTransform",
            "metadata": {"name": "grafana", "namespace": "team"},
            "spec": {"channels": ["alerts"], "title": "body.("},
        });
        let reviewed = handle(
            &admission(),
            review("CREATE", "NotifyTransform", transform, None),
        )
        .await;
        let (allowed, response) = answer(&reviewed);
        assert!(!allowed);
        assert_eq!(response["status"]["reason"], "InvalidSpec");
    }

    #[tokio::test]
    async fn deletes_and_unchanged_specs_pass_other_kinds_fail_closed() {
        let mut bad = spec();
        bad["image"] = json!({"tag": "9.9.9"});
        let object = instance(bad);
        let admission = admission();
        assert!(
            answer(
                &handle(
                    &admission,
                    review("DELETE", "NotifyLocalInstance", object.clone(), None)
                )
                .await
            )
            .0
        );
        assert!(
            answer(
                &handle(
                    &admission,
                    review(
                        "UPDATE",
                        "NotifyLocalInstance",
                        object.clone(),
                        Some(object.clone())
                    )
                )
                .await
            )
            .0
        );
        assert!(!answer(&handle(&admission, review("CREATE", "Other", object, None)).await).0);
    }
}
