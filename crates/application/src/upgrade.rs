//! Which Notify tags an instance may run: a version of
//! [`api::notify::SUPPORTED_VERSIONS`] (the operator renders that
//! version's configuration model), never below the version its database
//! was migrated to, never more than one minor ahead of it — unless
//! `notify.weebo.io/approved-upgrade` names that tag.

/// A `major.minor.patch` tag (a leading `v` allowed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn parse(tag: &str) -> Option<Self> {
        let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.');
        let mut next = || parts.next()?.parse::<u64>().ok();
        let version = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        parts.next().is_none().then_some(version)
    }
}

/// Why the instance may not run `to`, and how to allow it anyway.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UpgradeRefused(pub String);

/// Why the instance may not run `to` while its database is at `migrated`
/// (`None`: a new instance), unless `approved` names `to`.
pub fn check(
    migrated: Option<&str>,
    to: &str,
    approved: Option<&str>,
) -> Result<(), UpgradeRefused> {
    if approved == Some(to) {
        return Ok(());
    }
    let lift = format!(
        "annotate the instance {}={to} to run it anyway",
        api::annotations::APPROVED_UPGRADE
    );
    if !api::notify::SUPPORTED_VERSIONS.contains(&to) {
        return Err(UpgradeRefused(format!(
            "Notify {to} isn't a version this operator supports ({}): {lift}",
            api::notify::SUPPORTED_VERSIONS.join(", ")
        )));
    }
    let (Some(migrated), Some(target)) = (migrated, Version::parse(to)) else {
        return Ok(());
    };
    let Some(current) = Version::parse(migrated) else {
        return Ok(());
    };
    if target < current {
        return Err(UpgradeRefused(format!(
            "Notify {to} is older than the {migrated} the database was migrated to: it may not \
             read that schema; {lift}"
        )));
    }
    if target.major != current.major || target.minor > current.minor.saturating_add(1) {
        return Err(UpgradeRefused(format!(
            "Notify {migrated} → {to} skips a minor version: upgrade one minor at a time, or {lift}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = api::notify::VERSION;

    #[test]
    fn a_supported_tag_runs_and_others_need_approval() {
        assert!(check(None, V, None).is_ok());
        assert!(check(Some(V), V, None).is_ok());
        let refused = check(None, "9.9.9", None).unwrap_err();
        assert!(refused.0.contains("approved-upgrade=9.9.9"), "{refused}");
        assert!(check(None, "9.9.9", Some("9.9.9")).is_ok());
        assert!(check(None, "9.9.9", Some("9.9.8")).is_err());
    }

    #[test]
    fn never_below_the_migrated_version_nor_a_minor_skipped() {
        let current = Version::parse(V).unwrap();
        let newer = format!("{}.{}.0", current.major, current.minor.saturating_add(1));
        let skip = format!("{}.{}.0", current.major, current.minor.saturating_add(2));
        // The database ahead of the tag: refused.
        assert!(
            check(Some(&newer), V, None)
                .unwrap_err()
                .0
                .contains("older")
        );
        assert!(check(Some(V), &skip, Some(&skip)).is_ok());
        assert_eq!(Version::parse("v1.2.3").map(|v| v.minor), Some(2));
        assert_eq!(Version::parse("1.2"), None);
    }
}
