//! [`SecretStores`]: where `NotifyApiKey`s write their keys. Kubernetes
//! `Secret`s always; Vault when the chart configures it
//! (`secretStore.vault`, env `WEEBO_NOTIFY_SECRET_STORE`). The Vault
//! connection is the operator's, set by whoever installs it: never by an
//! instance, whose author could otherwise point the operator's Vault
//! identity at any path. Every object writes under
//! `<pathPrefix>/<its namespace>/`, with no grant out of it.

use std::sync::Arc;

use application::ports::{SecretStore, SecretStores};
use futures::FutureExt;
use kube::Client;
use serde::Deserialize;
use weebo_kit_adapters::{
    K8sSecretStore, RoutingSecretStore, VaultConfig, VaultLogins, VaultSecretStore, read_secret_key,
};
use weebo_kit_api::{SecretKeyRef, SecretStoreBackend};

/// The chart's `secretStore`, as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretStoreSettings {
    /// The backend of a key without `secretTargets`.
    #[serde(default)]
    pub default_backend: SecretStoreBackend,
    #[serde(default)]
    pub vault: Option<VaultSettings>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultSettings {
    /// Empty: no Vault.
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub mount: String,
    #[serde(default)]
    pub path_prefix: String,
    #[serde(default)]
    pub kubernetes_auth_role: String,
    #[serde(default)]
    pub kubernetes_auth_mount: String,
    /// A PEM bundle to verify Vault's TLS with (else the system's): the
    /// chart's `{}` (no `name`) is none.
    #[serde(default)]
    pub ca_secret_ref: CaSecretRef,
}

/// The chart's `secretStore.vault.caSecretRef`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CaSecretRef {
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub key: String,
}

impl CaSecretRef {
    fn get(&self) -> Option<SecretKeyRef> {
        (!self.name.trim().is_empty()).then(|| SecretKeyRef {
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            key: self.key.clone(),
        })
    }
}

impl SecretStoreSettings {
    /// `WEEBO_NOTIFY_SECRET_STORE`: unset or empty, Kubernetes only.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(Self::default());
        };
        let mut settings: Self = serde_json::from_str(value)
            .map_err(|e| format!("WEEBO_NOTIFY_SECRET_STORE is not a secretStore: {e}"))?;
        settings.vault = settings.vault.filter(|v| !v.address.trim().is_empty());
        if let Some(vault) = &settings.vault {
            let ca = &vault.ca_secret_ref;
            let mut required = vec![
                ("mount", &vault.mount),
                ("kubernetesAuthRole", &vault.kubernetes_auth_role),
                ("kubernetesAuthMount", &vault.kubernetes_auth_mount),
            ];
            if ca.get().is_some() {
                required.extend([
                    ("caSecretRef.namespace", &ca.namespace),
                    ("caSecretRef.key", &ca.key),
                ]);
            }
            for (field, value) in required {
                if value.trim().is_empty() {
                    return Err(format!(
                        "secretStore.vault.{field} is required with an address"
                    ));
                }
            }
        }
        if settings.default_backend == SecretStoreBackend::Vault && settings.vault.is_none() {
            return Err(
                "secretStore.defaultBackend is vault but secretStore.vault.address is empty".into(),
            );
        }
        Ok(settings)
    }
}

/// The store each pass writes through.
pub struct OperatorSecretStores {
    client: Client,
    kubernetes: Arc<K8sSecretStore>,
    settings: SecretStoreSettings,
    logins: Arc<VaultLogins>,
}

impl OperatorSecretStores {
    pub fn new(client: Client, settings: SecretStoreSettings) -> Self {
        Self {
            kubernetes: Arc::new(K8sSecretStore::new(
                client.clone(),
                crate::FIELD_MANAGER,
                api::MANAGED_BY,
                api::SECRET_OWNER_ANNOTATION,
            )),
            client,
            settings,
            logins: Arc::new(VaultLogins::default()),
        }
    }
}

/// Logs in to the operator's Vault (cached by `logins`). The CA bundle is
/// read first: its digest is part of the cache key, so a rotated CA is
/// picked up at once.
async fn vault(
    client: Client,
    logins: Arc<VaultLogins>,
    vault: VaultSettings,
) -> Result<Arc<VaultSecretStore>, String> {
    let ca_ref = vault.ca_secret_ref.get();
    let ca = match &ca_ref {
        Some(r) => Some(read_secret_key(&client, r).await?.ok_or_else(|| {
            format!(
                "secretStore.vault.caSecretRef {}/{} has no key {:?}",
                r.namespace, r.name, r.key
            )
        })?),
        None => None,
    };
    let config = VaultConfig {
        address: vault.address.clone(),
        mount: vault.mount.clone(),
        path_prefix: vault.path_prefix.clone(),
        // Never out of a namespace's subtree.
        path_grants: Vec::new(),
        kubernetes_auth_role: vault.kubernetes_auth_role.clone(),
        kubernetes_auth_mount: vault.kubernetes_auth_mount.clone(),
        ca_source: ca_ref
            .as_ref()
            .zip(ca.as_ref())
            .map(|(r, pem)| VaultConfig::ca_source_of(r, pem)),
    };
    logins
        .get("operator", &config, || async move { Ok(ca) })
        .await
        .map_err(|e| e.to_string())
}

impl SecretStores for OperatorSecretStores {
    fn store(&self) -> Arc<dyn SecretStore> {
        let backend = self.settings.default_backend;
        let Some(settings) = self.settings.vault.clone() else {
            return Arc::new(RoutingSecretStore::new(
                self.kubernetes.clone(),
                Err("the operator has no Vault: its chart's secretStore.vault is unset".into()),
                backend,
            ));
        };
        let (client, logins) = (self.client.clone(), self.logins.clone());
        // Only logged in to when a target routes to Vault: a broken Vault
        // never blocks Kubernetes targets.
        Arc::new(RoutingSecretStore::lazy(
            self.kubernetes.clone(),
            Box::new(move || vault(client, logins, settings).boxed()),
            backend,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chart_value_reads_as_settings() {
        assert_eq!(
            SecretStoreSettings::parse(None).unwrap(),
            SecretStoreSettings::default()
        );
        assert_eq!(
            SecretStoreSettings::parse(Some(" ")).unwrap(),
            SecretStoreSettings::default()
        );
        // The chart's defaults: no address, no Vault.
        let off = SecretStoreSettings::parse(Some(
            r#"{"defaultBackend":"kubernetes","vault":{"address":"","mount":"secret",
            "pathPrefix":"weebo-notify","kubernetesAuthRole":"","kubernetesAuthMount":"kubernetes",
            "caSecretRef":{}}}"#,
        ))
        .unwrap();
        assert_eq!(off.vault, None);
        let on = SecretStoreSettings::parse(Some(
            r#"{"defaultBackend":"vault","vault":{"address":"https://vault:8200","mount":"secret",
            "pathPrefix":"weebo-notify","kubernetesAuthRole":"weebo-notify",
            "kubernetesAuthMount":"kubernetes",
            "caSecretRef":{"namespace":"weebo-notify-system","name":"vault-ca","key":"ca.crt"}}}"#,
        ))
        .unwrap();
        assert_eq!(on.default_backend, SecretStoreBackend::Vault);
        let vault = on.vault.unwrap();
        assert_eq!(vault.kubernetes_auth_role, "weebo-notify");
        assert_eq!(vault.ca_secret_ref.get().unwrap().name, "vault-ca");
    }

    #[test]
    fn an_incomplete_vault_stops_the_start() {
        let no_role = SecretStoreSettings::parse(Some(
            r#"{"vault":{"address":"https://vault:8200","mount":"secret","kubernetesAuthMount":"kubernetes"}}"#,
        ));
        assert!(no_role.unwrap_err().contains("kubernetesAuthRole"));
        let ca_without_key = SecretStoreSettings::parse(Some(
            r#"{"vault":{"address":"https://vault:8200","mount":"secret","kubernetesAuthRole":"r",
            "kubernetesAuthMount":"kubernetes","caSecretRef":{"namespace":"ns","name":"ca"}}}"#,
        ));
        assert!(ca_without_key.unwrap_err().contains("caSecretRef.key"));
        let no_vault = SecretStoreSettings::parse(Some(r#"{"defaultBackend":"vault"}"#));
        assert!(no_vault.unwrap_err().contains("defaultBackend"));
        assert!(SecretStoreSettings::parse(Some("not json")).is_err());
    }
}
