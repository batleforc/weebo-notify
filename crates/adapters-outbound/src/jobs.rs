//! [`Jobs`] on the Kubernetes API: where a migration `Job` stands, from
//! its conditions.

use application::ports::{ApiError, JobState, Jobs};
use k8s_openapi::api::batch::v1::Job;
use kube::Client;
use kube::api::Api;

pub struct KubeJobs {
    client: Client,
}

impl KubeJobs {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl Jobs for KubeJobs {
    async fn job(&self, namespace: &str, name: &str) -> Result<Option<JobState>, ApiError> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), namespace);
        let Some(job) = jobs
            .get_opt(name)
            .await
            .map_err(crate::api_error(format_args!("Job {namespace}/{name}")))?
        else {
            return Ok(None);
        };
        Ok(Some(state(&job)))
    }
}

/// A `Job`'s state from its status.
pub fn state(job: &Job) -> JobState {
    let status = job.status.clone().unwrap_or_default();
    let condition = |kind: &str| {
        status
            .conditions
            .iter()
            .flatten()
            .find(|c| c.type_ == kind && c.status == "True")
    };
    JobState {
        active: status.active.unwrap_or(0) > 0,
        succeeded: condition("Complete").is_some(),
        failed: condition("Failed").map(|c| {
            [c.reason.as_deref(), c.message.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": ")
        }),
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

    use super::*;

    fn job(kind: &str, reason: Option<&str>) -> Job {
        Job {
            status: Some(JobStatus {
                conditions: Some(vec![JobCondition {
                    type_: kind.into(),
                    status: "True".into(),
                    reason: reason.map(String::from),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_job_reads_from_its_conditions() {
        assert!(state(&job("Complete", None)).succeeded);
        let failed = state(&job("Failed", Some("BackoffLimitExceeded")));
        assert_eq!(failed.failed.as_deref(), Some("BackoffLimitExceeded"));
        assert!(!failed.succeeded);
        assert_eq!(state(&Job::default()), JobState::default());
    }
}
