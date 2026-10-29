//! Single source of truth for every reason the operator surfaces. Status
//! conditions, Kubernetes Events and metrics all carry one of these
//! variants — never an ad-hoc string at the call site. Add a variant here
//! before reporting a new cause anywhere.

pub use weebo_kit_domain::{Reason, Severity};

weebo_kit_domain::reason_codes! {
    pub enum ReasonCode {
        /// `Ready: True` — the one success reason every reconciler uses.
        Reconciled,
        /// The spec renders a configuration Notify would refuse at load (a
        /// value out of range, a duration that doesn't parse, a CEL
        /// expression that doesn't compile, a feed whose publisher may not
        /// publish), or `extraConfig` sets what the CRs own. Only a change
        /// of the spec clears it.
        InvalidSpec,
        /// `spec.instanceRef` names no `NotifyLocalInstance` the operator
        /// sees (or, unset, there is none in the namespace to default to).
        InstanceNotFound,
        /// `spec.instanceRef` is unset and several instances could serve
        /// the object.
        InstanceAmbiguous,
        /// The instance named is out of reach: one of another namespace
        /// that doesn't accept objects of this one
        /// (`objectSelector.namespaces`), or outside the namespace the
        /// operator is limited to.
        InstanceNotAllowed,
        /// `spec.instanceRef` by name now designates another instance than
        /// the one recorded in `status.instance`: not followed until
        /// `spec.instanceRef.namespace` says which.
        InstanceBindingChanged,
        /// A `Secret` the operator would read, or the instance's pods
        /// would mount, lacks the label `notify.weebo.io/credential:
        /// "true"`.
        SecretNotAllowed,
        /// A referenced `Secret`, or its key, is missing.
        SecretMissing,
        /// An object the operator would create exists and isn't the
        /// instance's (no owner reference, no `managed-by` label): never
        /// taken over.
        OwnedObjectConflict,
        /// The configuration was written and the instance's pods haven't
        /// all confirmed loading it yet (a ConfigMap takes up to a minute
        /// to reach them).
        ConfigPropagating,
        /// Another object of the same kind on the instance holds the same
        /// id: the oldest (`creationTimestamp`, then `namespace/name`) is
        /// rendered, this one is left out.
        IdConflict,
        /// Notify's guard (`reconcile.maxRemovals`) refused this object's
        /// removal: it keeps its finalizer until the guard lets it go
        /// (`reconcile.allowMassRemoval`, or fewer removals at once).
        RemovalRefused,
        /// The image's tag isn't a Notify version the operator supports,
        /// or goes back below `status.version`, or jumps more than one
        /// minor: it needs `notify.weebo.io/approved-upgrade` set to that
        /// tag.
        UnsupportedVersion,
        /// The migration `Job` of a new version runs: the pods stay on the
        /// previous one until it succeeds.
        Migrating,
        /// The migration `Job` failed: the pods stay on the previous
        /// version.
        MigrationFailed,
        /// The server's pods aren't all ready on the desired image and boot
        /// configuration yet (rolling out, or not scheduled).
        WorkloadNotReady,
        /// `serviceMonitor.enabled` but the cluster has no `ServiceMonitor`
        /// kind (no Prometheus operator): none is created (advisory).
        MonitoringUnavailable,
        /// The instance is being deleted while channels, service accounts,
        /// transforms or trust policies are still bound to it: it waits
        /// for them.
        InstanceInUse,
        /// A pod reports a change only a restart applies although its boot
        /// configuration is current: the operator's boot/hot split missed
        /// a key (advisory; restart the pods to apply it).
        RestartPending,
        /// The web UI is on but nothing exposes the API to browsers (no
        /// `server.publicUrl`, no `ingress`): the UI points at the
        /// in-cluster `Service` (advisory).
        ApiNotExposed,
        /// A Kubernetes API call of the operator itself failed (reading a
        /// CR or a Secret, applying an object, patching a status) —
        /// retried.
        KubernetesApiError,
        /// A transform names a channel id no `NotifyChannel` of the
        /// instance declares: it may be a dynamic channel (advisory).
        ChannelNotManaged,
        /// An update would take a CR still carrying the finalizer out of
        /// the operator's label selector (`watch.labelSelector`): refused
        /// at admission.
        LeavesWatchScope,
        /// The pods loaded a new configuration (Event only).
        ConfigApplied,
        /// The pods rolled onto a new boot configuration or image (Event
        /// only).
        ConfigRolledOut,
        /// The database was migrated to a new version (Event only).
        Migrated,
        /// The finalizer finished — cleanup done, or the CR force-orphaned
        /// (Event only).
        Released,
    }
}

impl Reason for ReasonCode {
    const SUCCESS: Self = ReasonCode::Reconciled;

    fn as_str(self) -> &'static str {
        ReasonCode::as_str(self)
    }

    fn severity(self) -> Severity {
        ReasonCode::severity(self)
    }
}

impl ReasonCode {
    /// Advisory codes don't block `Ready` (and count as `success` in the
    /// reconcile metrics); every failure cause is blocking.
    pub const fn severity(self) -> Severity {
        match self {
            ReasonCode::Reconciled
            | ReasonCode::ChannelNotManaged
            | ReasonCode::MonitoringUnavailable
            | ReasonCode::RestartPending
            | ReasonCode::ApiNotExposed
            | ReasonCode::ConfigApplied
            | ReasonCode::ConfigRolledOut
            | ReasonCode::Migrated
            | ReasonCode::Released => Severity::Advisory,
            _ => Severity::Blocking,
        }
    }

    /// Failures only a change of the spec (or of what it references) can
    /// clear: retried on the steady interval rather than every 30 s. Each
    /// is retried right away anyway by the change that clears it (the
    /// kit's predicates, the watches).
    pub const fn needs_a_change(self) -> bool {
        matches!(
            self,
            ReasonCode::InvalidSpec
                | ReasonCode::InstanceAmbiguous
                | ReasonCode::InstanceNotAllowed
                | ReasonCode::InstanceBindingChanged
                | ReasonCode::SecretNotAllowed
                | ReasonCode::OwnedObjectConflict
                | ReasonCode::IdConflict
                | ReasonCode::UnsupportedVersion
                | ReasonCode::MigrationFailed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_is_pascal_case() {
        weebo_kit_domain::reason::assert_pascal_case(ReasonCode::ALL);
    }

    #[test]
    fn only_success_advisories_and_lifecycle_events_are_advisory() {
        let advisory: Vec<_> = ReasonCode::ALL
            .iter()
            .filter(|c| c.severity() == Severity::Advisory)
            .collect();
        assert_eq!(
            advisory,
            [
                &ReasonCode::Reconciled,
                &ReasonCode::MonitoringUnavailable,
                &ReasonCode::RestartPending,
                &ReasonCode::ApiNotExposed,
                &ReasonCode::ChannelNotManaged,
                &ReasonCode::ConfigApplied,
                &ReasonCode::ConfigRolledOut,
                &ReasonCode::Migrated,
                &ReasonCode::Released
            ]
        );
    }

    #[test]
    fn only_failures_wait_for_a_change() {
        for code in ReasonCode::ALL {
            if code.needs_a_change() {
                assert_eq!(code.severity(), Severity::Blocking, "{code}");
            }
        }
        assert!(!ReasonCode::KubernetesApiError.needs_a_change());
        assert!(!ReasonCode::ConfigPropagating.needs_a_change());
    }
}
