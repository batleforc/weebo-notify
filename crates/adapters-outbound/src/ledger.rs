//! [`StatusLedger`] on the Kubernetes API: the status fields an instance's
//! pass records right after a write (the kit's `status::record`, a merge
//! patch retried while the API server is unavailable).

use api::LocalInstance;
use application::ports::{ApiError, StatusLedger};
use kube::Client;
use kube::api::Api;

pub struct KubeInstanceLedger {
    client: Client,
}

impl KubeInstanceLedger {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl StatusLedger for KubeInstanceLedger {
    async fn record(
        &self,
        namespace: &str,
        name: &str,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), ApiError> {
        let api: Api<LocalInstance> = Api::namespaced(self.client.clone(), namespace);
        weebo_kit_runtime::status::record(&api, crate::FIELD_MANAGER, name, &fields)
            .await
            .map(|_| ())
            .map_err(crate::api_error(format_args!(
                "recording the status of {namespace}/{name}"
            )))
    }
}
