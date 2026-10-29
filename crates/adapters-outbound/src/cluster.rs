//! What an instance's pass reads of the cluster besides its own objects:
//! the Secrets its spec and its objects reference (only those labelled
//! `notify.weebo.io/credential: "true"`), and fresh keys.

use application::ports::{CredentialError, Credentials, KeyGenerator};
use base64::Engine;
use k8s_openapi::api::core::v1::Secret;
use kube::api::Api;
use kube::{Client, ResourceExt};

/// [`Credentials`] on the Kubernetes API.
pub struct KubeCredentials {
    client: Client,
}

impl KubeCredentials {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// The Secret, read only when it consents.
    async fn consenting(&self, namespace: &str, name: &str) -> Result<Secret, CredentialError> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let secret = api
            .get_opt(name)
            .await
            .map_err(|e| CredentialError::Kubernetes(format!("secret {namespace}/{name}: {e}")))?
            .ok_or_else(|| {
                CredentialError::Missing(format!("secret {namespace}/{name} not found"))
            })?;
        if secret
            .labels()
            .get(api::CREDENTIAL_LABEL)
            .map(String::as_str)
            != Some("true")
        {
            return Err(CredentialError::NotAllowed(
                weebo_kit_application::pod_safety::secret_not_labelled(
                    namespace,
                    name,
                    api::CREDENTIAL_LABEL,
                ),
            ));
        }
        Ok(secret)
    }
}

fn missing_key(namespace: &str, name: &str, key: &str) -> CredentialError {
    CredentialError::Missing(format!("secret {namespace}/{name} has no key {key:?}"))
}

#[async_trait::async_trait]
impl Credentials for KubeCredentials {
    async fn check(&self, namespace: &str, name: &str, key: &str) -> Result<(), CredentialError> {
        let secret = self.consenting(namespace, name).await?;
        let present = secret.data.as_ref().is_some_and(|d| d.contains_key(key))
            || secret
                .string_data
                .as_ref()
                .is_some_and(|d| d.contains_key(key));
        if present {
            Ok(())
        } else {
            Err(missing_key(namespace, name, key))
        }
    }

    async fn read(
        &self,
        namespace: &str,
        name: &str,
        key: &str,
    ) -> Result<String, CredentialError> {
        let secret = self.consenting(namespace, name).await?;
        let bytes = secret
            .data
            .as_ref()
            .and_then(|d| d.get(key))
            .ok_or_else(|| missing_key(namespace, name, key))?;
        String::from_utf8(bytes.0.clone()).map_err(|_| {
            CredentialError::Missing(format!(
                "secret {namespace}/{name}: key {key:?} is not UTF-8"
            ))
        })
    }
}

/// Keys from the OS's random source.
pub struct RandomKeys;

impl KeyGenerator for RandomKeys {
    #[expect(
        clippy::expect_used,
        reason = "getrandom fails only without an OS entropy source: no key beats a weak one"
    )]
    fn key(&self) -> String {
        let mut key = [0u8; 48];
        getrandom::fill(&mut key).expect("the OS provides randomness");
        base64::engine::general_purpose::STANDARD.encode(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_48_random_bytes() {
        let decode = |k: String| base64::engine::general_purpose::STANDARD.decode(k).unwrap();
        let (a, b) = (decode(RandomKeys.key()), decode(RandomKeys.key()));
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
        assert_eq!(RandomKeys.key().len(), 64);
    }
}
