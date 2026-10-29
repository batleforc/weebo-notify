//! The operator's [`InstanceDirectory`] over its only instance kind, the
//! namespaced `NotifyLocalInstance` (the kit's `LocalInstance` role): the
//! kit's `LocalInstances`, an instance shared with the objects of other
//! namespaces through its `objectSelector`
//! ([`api::local_instance::object_sharing`]) and with nothing else.

use api::LocalInstance;
use api::local_instance::object_sharing;
use application::ports::{ApiError, BoundInstance, InstanceDirectory, InstanceError};
use kube::Client;
use kube::core::Selector;
use kube::runtime::reflector::Store;
use weebo_kit_adapters::LocalInstances;
use weebo_kit_api::InstanceRef;

pub struct LocalInstanceDirectory {
    instances: LocalInstances<LocalInstance>,
}

impl LocalInstanceDirectory {
    /// Read from the API on every call (tests): wire the binary's
    /// reflector in with [`Self::with_store`].
    pub fn new(client: Client) -> Self {
        Self {
            instances: LocalInstances::new(client),
        }
    }

    pub fn with_store(mut self, store: Store<LocalInstance>) -> Self {
        self.instances = self.instances.with_store(store);
        self
    }

    /// The operator's watch scope: the namespace it is limited to and its
    /// label selector.
    pub fn with_scope(
        mut self,
        watched_namespace: Option<String>,
        selector: Option<Selector>,
    ) -> Self {
        self.instances = self.instances.with_scope(watched_namespace, selector);
        self
    }

    /// The instance `bound` designates, `None` when it is gone (or out of
    /// scope).
    pub async fn get(&self, bound: &BoundInstance) -> Result<Option<LocalInstance>, ApiError> {
        self.instances
            .get(bound)
            .await
            .map_err(|e| crate::api_error(e.what)(*e.source))
    }

    /// Resolves an object of `namespace`: an instance of another namespace
    /// serves it when its `objectSelector.namespaces` accepts that
    /// namespace.
    pub async fn resolve_attached(
        &self,
        namespace: &str,
        instance_ref: Option<&InstanceRef>,
        recorded: Option<&BoundInstance>,
    ) -> Result<BoundInstance, InstanceError> {
        self.instances
            .resolve_with(Some(namespace), instance_ref, recorded, object_sharing)
            .await
    }
}

#[async_trait::async_trait]
impl InstanceDirectory for LocalInstanceDirectory {
    /// The CR's own namespace only.
    async fn resolve(
        &self,
        cr_namespace: Option<&str>,
        instance_ref: Option<&InstanceRef>,
        recorded: Option<&BoundInstance>,
    ) -> Result<BoundInstance, InstanceError> {
        self.instances
            .resolve(cr_namespace, instance_ref, recorded)
            .await
    }
}

#[async_trait::async_trait]
impl application::ports::AttachedInstances for LocalInstanceDirectory {
    async fn resolve_attached(
        &self,
        namespace: &str,
        instance_ref: Option<&InstanceRef>,
        recorded: Option<&BoundInstance>,
    ) -> Result<BoundInstance, InstanceError> {
        LocalInstanceDirectory::resolve_attached(self, namespace, instance_ref, recorded).await
    }

    async fn instance(&self, bound: &BoundInstance) -> Result<Option<LocalInstance>, ApiError> {
        self.get(bound).await
    }
}
