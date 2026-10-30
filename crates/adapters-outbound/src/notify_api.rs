//! [`KeyIssuer`] over Notify's HTTP API: `POST`, `GET` and
//! `DELETE /api/v1/apikeys`, as the instance's key issuer (ADR-0033), on
//! the instance's `Service` (its `NetworkPolicy` lets the operator's pods
//! in on that port).

use std::time::Duration;

use api::common::Scope;
use application::ports::{IssuedKey, KeyIssuer, MintedKey, NewKey, NotifyApiError};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

/// How long a key call may take: one database round-trip and an Argon2
/// hash on Notify's side.
pub const API_TIMEOUT: Duration = Duration::from_secs(15);

/// Who a key acts as, as Notify spells it (`{"owner": "service_account",
/// "id": …}`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "owner", rename_all = "snake_case")]
enum Owner {
    ServiceAccount { id: String },
    User { id: String },
    Channel { id: String },
}

/// A key as `GET /apikeys` and `POST /apikeys` show it.
#[derive(Debug, Deserialize)]
struct Key {
    id: String,
    prefix: String,
    #[serde(default)]
    label: Option<String>,
    owner: Owner,
    scopes: Vec<Scope>,
    channels: Vec<String>,
    #[serde(default)]
    revoked_at: Option<String>,
}

impl From<Key> for IssuedKey {
    fn from(key: Key) -> Self {
        IssuedKey {
            id: key.id,
            prefix: key.prefix,
            label: key.label,
            service_account: match key.owner {
                Owner::ServiceAccount { id } => Some(id),
                Owner::User { .. } | Owner::Channel { .. } => None,
            },
            scopes: key.scopes,
            channels: key.channels,
            revoked: key.revoked_at.is_some(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Minted {
    #[serde(flatten)]
    key: Key,
    secret: String,
}

#[derive(Debug, Serialize)]
struct CreateKey<'a> {
    owner: Owner,
    label: &'a str,
    scopes: &'a [Scope],
    channels: &'a [String],
}

pub struct HttpKeyIssuer {
    http: reqwest::Client,
}

impl HttpKeyIssuer {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(API_TIMEOUT)
                .connect_timeout(API_TIMEOUT)
                // In-cluster: never through an HTTP(S)_PROXY.
                .no_proxy()
                .build()?,
        })
    }
}

fn url(endpoint: &str, path: &str) -> String {
    format!("{}/api/v1{path}", endpoint.trim_end_matches('/'))
}

/// A response's status as the port's error, with Notify's problem body.
async fn checked(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<reqwest::Response, NotifyApiError> {
    let response = response.map_err(|e| NotifyApiError::Failed(e.to_string()))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = format!("HTTP {status}: {}", body.trim());
    Err(match status {
        StatusCode::UNAUTHORIZED => NotifyApiError::Unauthorized(message),
        s if s.is_client_error() && s != StatusCode::TOO_MANY_REQUESTS => {
            NotifyApiError::Refused(message)
        }
        _ => NotifyApiError::Failed(message),
    })
}

async fn json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, NotifyApiError> {
    let body = response
        .text()
        .await
        .map_err(|e| NotifyApiError::Failed(e.to_string()))?;
    serde_json::from_str(&body)
        .map_err(|e| NotifyApiError::Failed(format!("an answer that doesn't read: {e}")))
}

#[async_trait::async_trait]
impl KeyIssuer for HttpKeyIssuer {
    async fn list(&self, endpoint: &str, token: &str) -> Result<Vec<IssuedKey>, NotifyApiError> {
        let response = self
            .http
            .get(url(endpoint, "/apikeys"))
            .bearer_auth(token)
            .send()
            .await;
        let keys: Vec<Key> = json(checked(response).await?).await?;
        Ok(keys.into_iter().map(IssuedKey::from).collect())
    }

    async fn mint(
        &self,
        endpoint: &str,
        token: &str,
        key: &NewKey,
    ) -> Result<MintedKey, NotifyApiError> {
        let body = CreateKey {
            owner: Owner::ServiceAccount {
                id: key.service_account.clone(),
            },
            label: &key.label,
            scopes: &key.scopes,
            channels: &key.channels,
        };
        let response = self
            .http
            .post(url(endpoint, "/apikeys"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await;
        let minted: Minted = json(checked(response).await?).await?;
        Ok(MintedKey {
            key: minted.key.into(),
            secret: minted.secret,
        })
    }

    async fn revoke(&self, endpoint: &str, token: &str, id: &str) -> Result<(), NotifyApiError> {
        let response = self
            .http
            .delete(url(endpoint, &format!("/apikeys/{id}")))
            .bearer_auth(token)
            .send()
            .await;
        checked(response).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_as_notify_lists_them() {
        let body = r#"[{"id":"key_01J","prefix":"a1b2c3d4","label":"weebo-notify:team/ci",
            "owner":{"owner":"service_account","id":"ci-bot"},"scopes":["publish"],
            "channels":["alerts"],"created_at":"2026-10-30T10:00:00Z","expires_at":null,
            "last_used_at":null,"revoked_at":null},
            {"id":"key_02J","prefix":"e5f6","owner":{"owner":"channel","id":"alerts"},
            "scopes":["subscribe"],"channels":["alerts"],"created_at":"2026-10-30T10:00:00Z",
            "revoked_at":"2026-10-30T11:00:00Z"}]"#;
        let keys: Vec<IssuedKey> = serde_json::from_str::<Vec<Key>>(body)
            .unwrap()
            .into_iter()
            .map(IssuedKey::from)
            .collect();
        assert_eq!(keys[0].service_account.as_deref(), Some("ci-bot"));
        assert_eq!(keys[0].scopes, [Scope::Publish]);
        assert!(!keys[0].revoked);
        assert_eq!(keys[1].service_account, None);
        assert!(keys[1].revoked);
        let minted: Minted = serde_json::from_str(
            r#"{"id":"key_01J","prefix":"a1b2","owner":{"owner":"service_account","id":"ci"},
            "scopes":["publish"],"channels":["alerts"],"created_at":"2026-10-30T10:00:00Z",
            "secret":"wnk_x"}"#,
        )
        .unwrap();
        assert_eq!(minted.secret, "wnk_x");
    }

    #[test]
    fn a_mint_is_asked_as_notify_reads_it() {
        let body = CreateKey {
            owner: Owner::ServiceAccount { id: "ci".into() },
            label: "weebo-notify:team/ci",
            scopes: &[Scope::Publish],
            channels: &["alerts".to_string()],
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({
                "owner": {"owner": "service_account", "id": "ci"},
                "label": "weebo-notify:team/ci",
                "scopes": ["publish"],
                "channels": ["alerts"],
            })
        );
        assert_eq!(
            url("http://main.team.svc:8080/", "/apikeys"),
            "http://main.team.svc:8080/api/v1/apikeys"
        );
    }
}
