//! In-memory ports for the use-cases' tests (and other crates', with the
//! `test-support` feature): the kit's owned objects, and this operator's
//! credentials, object directory, status ledger and instances.

// Test doubles, only built for tests (`cfg(test)` or the `test-support`
// feature): a poisoned lock or a fixture that doesn't read fails the test.
#![expect(clippy::unwrap_used, reason = "test doubles: a panic is a failed test")]

use std::collections::BTreeMap;
use std::sync::Mutex;

use api::local_instance::ObjectSelector;
use api::status::InstanceStatus;
pub use weebo_kit_application::test_support::FakeOwnedObjects;

use crate::objects::ObjectView;
use crate::ports::{
    ApiError, AttachedInstances, BoundInstance, CredentialError, Credentials, InstanceError,
    ObjectDirectory, Owner, StatusLedger,
};

/// A `Secret`: whether it carries the credential label, and its keys.
type FakeSecret = (bool, BTreeMap<String, String>);

/// `Secret`s by `(namespace, name)`.
#[derive(Default)]
pub struct FakeCredentials {
    secrets: Mutex<BTreeMap<(String, String), FakeSecret>>,
}

impl FakeCredentials {
    /// A labelled `Secret`.
    pub fn insert(&self, namespace: &str, name: &str, data: &[(&str, &str)]) {
        self.put(namespace, name, true, data);
    }

    /// A `Secret` without the credential label.
    pub fn insert_unlabelled(&self, namespace: &str, name: &str, data: &[(&str, &str)]) {
        self.put(namespace, name, false, data);
    }

    fn put(&self, namespace: &str, name: &str, labelled: bool, data: &[(&str, &str)]) {
        self.secrets.lock().unwrap().insert(
            (namespace.into(), name.into()),
            (
                labelled,
                data.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        );
    }
}

#[async_trait::async_trait]
impl Credentials for FakeCredentials {
    async fn check(&self, namespace: &str, name: &str, key: &str) -> Result<(), CredentialError> {
        self.read(namespace, name, key).await.map(|_| ())
    }

    async fn read(
        &self,
        namespace: &str,
        name: &str,
        key: &str,
    ) -> Result<String, CredentialError> {
        let secrets = self.secrets.lock().unwrap();
        let (labelled, data) = secrets
            .get(&(namespace.into(), name.into()))
            .ok_or_else(|| {
                CredentialError::Missing(format!("secret {namespace}/{name} not found"))
            })?;
        if !labelled {
            return Err(CredentialError::NotAllowed(format!(
                "secret {namespace}/{name} lacks the label {}",
                api::CREDENTIAL_LABEL
            )));
        }
        data.get(key).cloned().ok_or_else(|| {
            CredentialError::Missing(format!("secret {namespace}/{name} has no key {key:?}"))
        })
    }
}

/// The objects bound to every instance.
#[derive(Default)]
pub struct FakeDirectory {
    views: Mutex<Vec<ObjectView>>,
}

impl FakeDirectory {
    pub fn with(views: Vec<ObjectView>) -> Self {
        Self {
            views: Mutex::new(views),
        }
    }
}

#[async_trait::async_trait]
impl ObjectDirectory for FakeDirectory {
    async fn bound_to(
        &self,
        _instance: &Owner,
        _selector: &ObjectSelector,
    ) -> Result<Vec<ObjectView>, ApiError> {
        Ok(self.views.lock().unwrap().clone())
    }
}

/// Every status record, in order.
#[derive(Default)]
pub struct FakeLedger {
    records: Mutex<Vec<serde_json::Map<String, serde_json::Value>>>,
}

impl FakeLedger {
    pub fn last(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        self.records.lock().unwrap().last().cloned()
    }
}

#[async_trait::async_trait]
impl StatusLedger for FakeLedger {
    async fn record(
        &self,
        _namespace: &str,
        _name: &str,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), ApiError> {
        self.records.lock().unwrap().push(fields);
        Ok(())
    }
}

/// One instance, `main` of the object's namespace, with the status given
/// (none: it is gone).
#[derive(Default)]
pub struct FakeInstances {
    instance: Mutex<Option<api::LocalInstance>>,
}

impl FakeInstances {
    pub fn with_status(status: InstanceStatus) -> Self {
        let spec = serde_json::from_value(serde_json::json!({
            "database": {"urlSecretRef": {"name": "db", "key": "url"}},
            "attachments": {"s3": {"endpoint": "http://s3", "bucket": "b",
                                   "credentialsSecretRef": {"name": "s3"}}},
            "oidc": {"issuer": "https://id.example", "clientId": "notify"},
        }))
        .unwrap();
        let mut instance = api::LocalInstance::new("main", spec);
        instance.status = Some(status);
        Self {
            instance: Mutex::new(Some(instance)),
        }
    }
}

#[async_trait::async_trait]
impl AttachedInstances for FakeInstances {
    async fn resolve_attached(
        &self,
        namespace: &str,
        _instance_ref: Option<&weebo_kit_api::InstanceRef>,
        _recorded: Option<&BoundInstance>,
    ) -> Result<BoundInstance, InstanceError> {
        Ok(BoundInstance::local(namespace, "main"))
    }

    async fn instance(
        &self,
        _bound: &BoundInstance,
    ) -> Result<Option<api::LocalInstance>, ApiError> {
        Ok(self.instance.lock().unwrap().clone())
    }
}
