//! [`ServiceAccounts`]: the `NotifyServiceAccount`s API keys name, from
//! the binary's shared watch when it has one, else from the API.

use std::sync::Arc;

use application::ports::{AccountView, ApiError, ServiceAccounts};
use kube::Client;
use kube::api::Api;
use kube::runtime::reflector::{ObjectRef, Store};

pub struct KubeServiceAccounts {
    client: Client,
    store: Option<Store<api::ServiceAccount>>,
}

impl KubeServiceAccounts {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            store: None,
        }
    }

    pub fn with_store(mut self, store: Store<api::ServiceAccount>) -> Self {
        self.store = Some(store);
        self
    }
}

/// What an API key reads of its account.
pub fn view(account: &api::ServiceAccount) -> AccountView {
    let status = account.status.clone().unwrap_or_default();
    AccountView {
        id: status.id,
        instance: status.instance,
        ready: weebo_kit_api::status::ready(&status.conditions).is_some_and(|c| c.status == "True"),
    }
}

#[async_trait::async_trait]
impl ServiceAccounts for KubeServiceAccounts {
    async fn get(&self, namespace: &str, name: &str) -> Result<Option<AccountView>, ApiError> {
        let found: Option<Arc<api::ServiceAccount>> = match &self.store {
            Some(store) => store.get(&ObjectRef::new(name).within(namespace)),
            None => Api::<api::ServiceAccount>::namespaced(self.client.clone(), namespace)
                .get_opt(name)
                .await
                .map_err(crate::api_error(format_args!(
                    "reading NotifyServiceAccount {namespace}/{name}"
                )))?
                .map(Arc::new),
        };
        Ok(found
            .filter(|a| a.metadata.deletion_timestamp.is_none())
            .map(|a| view(&a)))
    }
}
