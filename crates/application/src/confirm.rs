//! Whether the instance's pods run the configuration last written, from
//! their own `/healthz` (ADR-0032): the digest every pod applied, the
//! removals Notify's guard refused, what only a restart would apply.
//!
//! Each replica applies the file in a reconcile pass of its own once it
//! sees it (the kubelet's `ConfigMap` sync, then Notify's poll): the file
//! is applied when *every* ready pod reports its digest, and there are at
//! least as many of them as the `Deployment` counts available — a pod not
//! asked can't confirm anything.

use std::collections::BTreeSet;

use crate::ports::Replica;

/// What the pods say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Confirmation {
    /// Every ready pod applied the file.
    pub applied: bool,
    /// Why not, when not.
    pub waiting: Option<String>,
    /// The guard's refused removals, over every pod that answered.
    pub refused_channels: Vec<String>,
    pub refused_service_accounts: Vec<String>,
    /// What only a restart applies, over every pod that answered.
    pub pending_restart: Vec<String>,
}

/// What `replicas` say of `digest`, the deployment counting `available`
/// pods.
pub fn confirm(digest: &str, replicas: &[Replica], available: i32) -> Confirmation {
    let mut refused_channels = BTreeSet::new();
    let mut refused_service_accounts = BTreeSet::new();
    let mut pending_restart = BTreeSet::new();
    let mut behind = Vec::new();
    for replica in replicas {
        match &replica.answer {
            Ok(applied) => {
                refused_channels.extend(applied.refused_channels.iter().cloned());
                refused_service_accounts.extend(applied.refused_service_accounts.iter().cloned());
                pending_restart.extend(applied.pending_restart.iter().cloned());
                if applied.digest.as_deref() != Some(digest) {
                    behind.push(format!("{} runs another file", replica.pod));
                }
            }
            Err(why) => behind.push(format!("{} didn't answer ({why})", replica.pod)),
        }
    }
    let asked = i32::try_from(replicas.len()).unwrap_or(i32::MAX);
    let waiting = if replicas.is_empty() {
        Some("no ready pod to ask".to_string())
    } else if !behind.is_empty() {
        Some(behind.join(", "))
    } else if asked < available {
        Some(format!("{asked} of {available} pods asked"))
    } else {
        None
    };
    Confirmation {
        applied: waiting.is_none(),
        waiting,
        refused_channels: refused_channels.into_iter().collect(),
        refused_service_accounts: refused_service_accounts.into_iter().collect(),
        pending_restart: pending_restart.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::AppliedConfig;

    fn replica(pod: &str, digest: Option<&str>) -> Replica {
        Replica {
            pod: pod.into(),
            answer: Ok(AppliedConfig {
                digest: digest.map(String::from),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn the_file_is_applied_once_every_pod_reports_it() {
        let all = [replica("a", Some("d")), replica("b", Some("d"))];
        assert!(confirm("d", &all, 2).applied);
        // One stale pod.
        let stale = [replica("a", Some("d")), replica("b", Some("old"))];
        let c = confirm("d", &stale, 2);
        assert!(!c.applied);
        assert!(c.waiting.unwrap().contains("b runs another file"));
        // One that doesn't answer, none at all, fewer than available.
        let silent = [
            replica("a", Some("d")),
            Replica {
                pod: "b".into(),
                answer: Err("timed out".into()),
            },
        ];
        assert!(!confirm("d", &silent, 2).applied);
        assert!(!confirm("d", &[], 0).applied);
        assert!(!confirm("d", &[replica("a", Some("d"))], 2).applied);
    }

    #[test]
    fn refused_removals_and_restarts_are_the_union_over_pods() {
        let mut a = AppliedConfig {
            digest: Some("d".into()),
            refused_channels: vec!["ci".into()],
            pending_restart: vec!["[admin] oidc_groups".into()],
            ..Default::default()
        };
        let mut b = a.clone();
        b.refused_channels = vec!["ops".into(), "ci".into()];
        b.refused_service_accounts = vec!["bot".into()];
        a.pending_restart.push("[auth]".into());
        let replicas = [
            Replica {
                pod: "a".into(),
                answer: Ok(a),
            },
            Replica {
                pod: "b".into(),
                answer: Ok(b),
            },
        ];
        let c = confirm("d", &replicas, 2);
        assert!(c.applied);
        assert_eq!(c.refused_channels, ["ci", "ops"]);
        assert_eq!(c.refused_service_accounts, ["bot"]);
        assert_eq!(c.pending_restart, ["[admin] oidc_groups", "[auth]"]);
    }
}
