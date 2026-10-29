//! [`ReplicaHealth`] on the cluster: the ready server pods (their IPs,
//! from the API) each asked `GET http://<ip>:8080/healthz` (ADR-0032:
//! unauthenticated, in memory, per replica), at once, with a short
//! timeout. The instance's `NetworkPolicy` lets the operator's pods in on
//! that port.

use std::collections::BTreeMap;
use std::time::Duration;

use application::ports::{ApiError, AppliedConfig, Replica, ReplicaHealth};
use application::render::paths;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams};
use kube::{Client, ResourceExt};
use serde::Deserialize;

/// How long a pod may take to answer: `/healthz` reads memory only.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// The part of `/healthz` the operator reads.
#[derive(Debug, Deserialize)]
struct Health {
    config: HealthConfig,
}

#[derive(Debug, Deserialize)]
struct HealthConfig {
    digest: Option<String>,
    #[serde(default)]
    pending_restart: Vec<String>,
    #[serde(default)]
    refused: Refused,
}

#[derive(Debug, Default, Deserialize)]
struct Refused {
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default)]
    service_accounts: Vec<String>,
}

/// A `/healthz` body as the port's [`AppliedConfig`].
pub fn parse(body: &str) -> Result<AppliedConfig, String> {
    let health: Health =
        serde_json::from_str(body).map_err(|e| format!("not a /healthz answer: {e}"))?;
    Ok(AppliedConfig {
        digest: health.config.digest,
        pending_restart: health.config.pending_restart,
        refused_channels: health.config.refused.channels,
        refused_service_accounts: health.config.refused.service_accounts,
    })
}

/// Where a pod answers: its IP (an IPv6 one bracketed) on Notify's port.
pub fn health_url(ip: &str) -> String {
    if ip.contains(':') {
        format!("http://[{ip}]:{}/healthz", paths::PORT)
    } else {
        format!("http://{ip}:{}/healthz", paths::PORT)
    }
}

/// Whether a pod serves: `Ready`, not being deleted, with an IP.
fn ready_ip(pod: &Pod) -> Option<String> {
    if pod.metadata.deletion_timestamp.is_some() {
        return None;
    }
    let status = pod.status.as_ref()?;
    let ready = status
        .conditions
        .iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True");
    ready.then(|| status.pod_ip.clone()).flatten()
}

pub struct KubeReplicaHealth {
    client: Client,
    http: reqwest::Client,
}

impl KubeReplicaHealth {
    pub fn new(client: Client) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client,
            http: reqwest::Client::builder()
                .timeout(HEALTH_TIMEOUT)
                .connect_timeout(HEALTH_TIMEOUT)
                // In-cluster: never through an HTTP(S)_PROXY.
                .no_proxy()
                .build()?,
        })
    }

    async fn ask(&self, url: &str) -> Result<AppliedConfig, String> {
        let response = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        parse(&response.text().await.map_err(|e| e.to_string())?)
    }
}

#[async_trait::async_trait]
impl ReplicaHealth for KubeReplicaHealth {
    async fn replicas(
        &self,
        namespace: &str,
        selector: &BTreeMap<String, String>,
    ) -> Result<Vec<Replica>, ApiError> {
        let labels = selector
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let listed = pods
            .list(&ListParams::default().labels(&labels))
            .await
            .map_err(crate::api_error(format_args!(
                "listing the pods of {namespace}"
            )))?;
        let asked = listed.items.iter().filter_map(|pod| {
            let ip = ready_ip(pod)?;
            let name = pod.name_any();
            Some(async move {
                Replica {
                    answer: self.ask(&health_url(&ip)).await,
                    pod: name,
                }
            })
        });
        Ok(futures::future::join_all(asked).await)
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};

    use super::*;

    #[test]
    fn a_healthz_answer_reads_as_its_applied_config() {
        let body = r#"{"status":"ok","version":"0.3.0","config":{
            "digest":"9f86d0","applied_at":"2026-10-29T14:02:11Z",
            "pending_restart":["[database] url — the pool is connected at boot"],
            "refused":{"channels":["ci-infra"],"service_accounts":[]}}}"#;
        let applied = parse(body).unwrap();
        assert_eq!(applied.digest.as_deref(), Some("9f86d0"));
        assert_eq!(applied.refused_channels, ["ci-infra"]);
        assert_eq!(applied.pending_restart.len(), 1);
        // Before the first pass.
        let none = parse(r#"{"status":"ok","version":"0.3.0","config":{"digest":null,
            "applied_at":null,"pending_restart":[],"refused":{"channels":[],"service_accounts":[]}}}"#)
        .unwrap();
        assert_eq!(none.digest, None);
        assert!(parse("ok").is_err());
    }

    #[test]
    fn only_ready_pods_with_an_ip_are_asked() {
        let pod = |ready: &str, ip: Option<&str>| Pod {
            status: Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "Ready".into(),
                    status: ready.into(),
                    ..Default::default()
                }]),
                pod_ip: ip.map(String::from),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            ready_ip(&pod("True", Some("10.0.0.7"))).as_deref(),
            Some("10.0.0.7")
        );
        assert_eq!(ready_ip(&pod("False", Some("10.0.0.7"))), None);
        assert_eq!(ready_ip(&pod("True", None)), None);
        assert_eq!(health_url("10.0.0.7"), "http://10.0.0.7:8080/healthz");
        assert_eq!(health_url("fd00::7"), "http://[fd00::7]:8080/healthz");
    }
}
