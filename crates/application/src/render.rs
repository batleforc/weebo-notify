//! An instance's `config.toml`, rendered from its spec and the objects
//! bound to it, then parsed and checked by Notify's own model
//! (`notify-config`, ADR-0031): what the operator writes is what Notify
//! loads.
//!
//! Pure: the Secrets the spec and the objects name are given as their
//! state ([`SecretStates`]), never as their values. The file holds no
//! secret: the pods get them as `WNK_*` variables ([`Rendered::env`]) or as
//! files a `file:` reference names ([`Rendered::files`]).
//!
//! Each object is checked on its own against the instance's file (a value
//! Notify would refuse leaves that object out, `Invalid`, not the whole
//! instance); then the whole file is parsed back and validated once more —
//! a failure there is the instance's (`InvalidSpec`).
//!
//! The boot/hot split is Notify's: [`notify_config::Config::reloaded_from`]
//! says what a reload adopts, so the boot hash is the hash of everything it
//! doesn't (plus the federation issuers, whose verifiers are built at
//! boot). It changes only when a restart is needed.

use std::collections::{BTreeMap, BTreeSet};

use api::local_instance::{LocalInstanceSpec, Names};
use api::status::{ObjectRender, RenderState};
use notify_config::Config;
use serde_json::{Value, json};
use weebo_kit_application::workload::content_hash;

use crate::objects::{self, ObjectKind, ObjectSpec, ObjectView};

/// Where things are in the server's pods.
pub mod paths {
    /// The `ConfigMap` key holding the file.
    pub const CONFIG_KEY: &str = "config.toml";
    /// Where the `ConfigMap` is mounted (a directory: a `subPath` mount
    /// would never see an update).
    pub const CONFIG_DIR: &str = "/etc/notify";
    pub const CONFIG_FILE: &str = "/etc/notify/config.toml";
    /// The port the server listens on (`[server] bind`).
    pub const PORT: u16 = 8080;
    /// The filesystem store's volume.
    pub const ATTACHMENTS_DIR: &str = "/var/lib/notify/attachments";
    /// The sinks' secrets, copied by the operator into `<name>-sinks` and
    /// mounted whole: a new one appears without a restart.
    pub const SINK_SECRETS_DIR: &str = "/var/run/notify/sinks";
    /// The VAPID key, mounted from its `Secret`.
    pub const VAPID_KEY_FILE: &str = "/var/run/notify/vapid/key";
    /// The key of the `Secret`s the operator generates.
    pub const GENERATED_KEY: &str = "key";
    /// The variable `[sinks.smtp] password_ref` names.
    pub const SMTP_PASSWORD_ENV: &str = "WNK_SMTP_PASSWORD";
}

/// A key of a `Secret`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretSource {
    pub namespace: String,
    pub name: String,
    pub key: String,
}

impl SecretSource {
    fn new(namespace: &str, name: &str, key: &str) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            key: key.into(),
        }
    }
}

impl std::fmt::Display for SecretSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "secret {}/{} key {:?}",
            self.namespace, self.name, self.key
        )
    }
}

/// A variable of the server's pods, from a `Secret` of the instance's
/// namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEnv {
    pub name: String,
    pub source: SecretSource,
    /// A `Secret` the operator generates (none named in the spec).
    pub generated: bool,
}

/// A file of the server's pods, holding a `Secret`'s key: one of the
/// instance's namespace is mounted, a sink's (of its channel's namespace)
/// is copied into `<name>-sinks` first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFile {
    pub path: String,
    pub source: SecretSource,
}

/// What a referenced `Secret` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretState {
    Present,
    /// It, or its key, is missing.
    Missing(String),
    /// It lacks the credential label.
    NotAllowed(String),
}

/// The state of the `Secret`s the objects name ([`object_secrets`]); one
/// absent from the map is taken as present.
pub type SecretStates = BTreeMap<SecretSource, SecretState>;

/// What the render is about.
pub struct Input<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
    pub spec: &'a LocalInstanceSpec,
    /// The objects bound to the instance, in any order.
    pub objects: &'a [ObjectView],
    pub secrets: &'a SecretStates,
    /// The previous render map: the generations the pods confirmed carry
    /// over.
    pub previous: &'a BTreeMap<String, ObjectRender>,
}

/// An instance's file and what goes with it.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The file as Notify reads it.
    pub config: Config,
    /// `config.toml`.
    pub text: String,
    /// The SHA-256 of `text`.
    pub config_digest: String,
    /// The hash of what Notify reads at boot only: a new one rolls the
    /// pods.
    pub boot_hash: String,
    pub env: Vec<SecretEnv>,
    pub files: Vec<SecretFile>,
    /// What became of each object (`<kind>/<namespace>/<name>`).
    pub objects: BTreeMap<String, ObjectRender>,
}

/// Why an instance can't be rendered: its own spec (or the whole file)
/// is one Notify would refuse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("{0}")]
    Invalid(String),
}

/// The keys of `config.toml` the CRs own, and the secrets: `extraConfig`
/// may set none of them.
const RESERVED: &[&str] = &[
    "channels.managed",
    "service_accounts",
    "transforms",
    "trust_policy",
    "database.url",
    "attachments.access_key",
    "attachments.secret_key",
    "attachments.token_secret",
    "sinks.fingerprint_key",
    "sinks.previous_fingerprint_keys",
];

/// The `Secret`s the instance's spec names (not those the operator
/// generates), in its namespace.
pub fn instance_secrets(namespace: &str, spec: &LocalInstanceSpec) -> Vec<SecretSource> {
    let mut out = vec![SecretSource::new(
        namespace,
        &spec.database.url_secret_ref.name,
        &spec.database.url_secret_ref.key,
    )];
    if let Some(s3) = &spec.attachments.s3 {
        let c = &s3.credentials_secret_ref;
        out.push(SecretSource::new(namespace, &c.name, &c.access_key_id_key));
        out.push(SecretSource::new(
            namespace,
            &c.name,
            &c.secret_access_key_key,
        ));
    }
    if let Some(r) = spec
        .attachments
        .filesystem
        .as_ref()
        .and_then(|f| f.token_secret_ref.as_ref())
    {
        out.push(SecretSource::new(namespace, &r.name, &r.key));
    }
    if let Some(r) = &spec.sinks.fingerprint_key_secret_ref {
        out.push(SecretSource::new(namespace, &r.name, &r.key));
    }
    if let Some(r) = spec
        .smtp
        .as_ref()
        .and_then(|s| s.password_secret_ref.as_ref())
    {
        out.push(SecretSource::new(namespace, &r.name, &r.key));
    }
    if let Some(push) = &spec.web_push {
        let r = &push.vapid_key_secret_ref;
        out.push(SecretSource::new(namespace, &r.name, &r.key));
    }
    out
}

/// The `Secret`s the objects name (a sink's `secretRef`), each in its
/// object's namespace, once each.
pub fn object_secrets(objects: &[ObjectView]) -> Vec<SecretSource> {
    let mut out = BTreeSet::new();
    for view in objects.iter().filter(|v| !v.deleting) {
        if let ObjectSpec::Channel(channel) = &view.spec {
            for r in channel.sinks.iter().filter_map(|s| s.secret_ref.as_ref()) {
                out.insert(SecretSource::new(&view.namespace, &r.name, &r.key));
            }
        }
    }
    out.into_iter().collect()
}

/// The file of a sink's secret under [`paths::SINK_SECRETS_DIR`] (and its
/// key in `<name>-sinks`): a hash of where it comes from, a valid `Secret`
/// key whatever the names.
pub fn sink_entry(source: &SecretSource) -> String {
    let hash =
        content_hash(format!("{}/{}/{}", source.namespace, source.name, source.key).as_bytes());
    format!("sink-{}", hash.get(..16).unwrap_or(&hash))
}

/// Drops the `null`s of a JSON value (TOML has none), recursively.
fn prune(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, prune(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(prune).collect()),
        other => other,
    }
}

fn table(value: Value) -> Result<toml::Table, String> {
    toml::Table::try_from(prune(value)).map_err(|e| format!("not TOML: {e}"))
}

/// The paths of the leaves of `table` (`a.b.c`), an array being a leaf.
fn leaves(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(t) => leaves(t, &path, out),
            _ => out.push(path),
        }
    }
}

fn has_path(table: &toml::Table, path: &str) -> bool {
    let mut current = table;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        match current.get(part) {
            None => return false,
            Some(toml::Value::Table(t)) if parts.peek().is_some() => current = t,
            Some(_) => return parts.peek().is_none(),
        }
    }
    true
}

/// `extra` merged into `base`, tables deep.
fn merge(base: &mut toml::Table, extra: toml::Table) {
    for (key, value) in extra {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(e)) => merge(b, e),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// `spec.extraConfig`, merged into the instance's own table: it may set
/// nothing a typed field renders, nor what the CRs own, nor a secret.
fn merge_extra(spec: &LocalInstanceSpec, base: &mut toml::Table) -> Result<(), Vec<String>> {
    if spec.extra_config.is_empty() {
        return Ok(());
    }
    let extra_value = Value::Object(
        spec.extra_config
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let extra = table(extra_value).map_err(|e| vec![format!("spec.extraConfig is {e}")])?;
    let mut paths = Vec::new();
    leaves(&extra, "", &mut paths);
    let mut problems = Vec::new();
    for path in paths {
        if let Some(reserved) = RESERVED
            .iter()
            .find(|r| path == **r || path.starts_with(&format!("{r}.")))
        {
            problems.push(format!(
                "spec.extraConfig sets {path}: {reserved} comes from the CRs (or a Secret) only"
            ));
        } else if has_path(base, &path) {
            problems.push(format!(
                "spec.extraConfig sets {path}, which the instance's spec renders"
            ));
        }
    }
    if problems.is_empty() {
        merge(base, extra);
        Ok(())
    } else {
        Err(problems)
    }
}

/// The instance's own table: everything but the objects.
fn instance_table(input: &Input<'_>) -> Result<toml::Table, Vec<String>> {
    let spec = input.spec;
    let mut problems = Vec::new();
    let mut cors = spec.server.cors_origins.clone();
    if spec.web.enabled
        && let Some(web) = spec.ingress.as_ref().and_then(|i| i.web.as_ref())
    {
        let origin = format!("https://{}", web.host);
        if !cors.contains(&origin) {
            cors.push(origin);
        }
    }
    let attachments = match (&spec.attachments.s3, &spec.attachments.filesystem) {
        (Some(s3), _) => json!({
            "kind": "s3",
            "endpoint": s3.endpoint,
            "bucket": s3.bucket,
            "region": s3.region,
        }),
        (None, Some(fs)) => json!({
            "kind": "filesystem",
            "path": paths::ATTACHMENTS_DIR,
            "public_base_url": fs.public_base_url,
        }),
        (None, None) => {
            problems.push("spec.attachments: one of s3 and filesystem is required".into());
            Value::Null
        }
    };
    let telemetry = match &spec.telemetry {
        None => Value::Null,
        Some(t) => {
            let ratio = match t.sample_ratio.as_deref().map(str::parse::<f64>) {
                None => None,
                Some(Ok(r)) if (0.0..=1.0).contains(&r) => Some(r),
                Some(_) => {
                    problems.push(format!(
                        "spec.telemetry.sampleRatio: {:?} is not a number from 0.0 to 1.0",
                        t.sample_ratio.as_deref().unwrap_or_default()
                    ));
                    None
                }
            };
            json!({
                "otlp_endpoint": t.otlp_endpoint,
                "service_name": t.service_name.clone().unwrap_or_else(|| input.name.to_string()),
                "sample_ratio": ratio,
            })
        }
    };
    let smtp = spec.smtp.as_ref().map(|s| {
        json!({
            "url": s.url,
            "from": s.from,
            "username": s.username,
            "password_ref": s.password_secret_ref.as_ref()
                .map(|_| format!("env:{}", paths::SMTP_PASSWORD_ENV)),
            "permalink_base": s.permalink_base,
        })
    });
    let push = spec.web_push.as_ref().map(
        |p| json!({"webpush": {"vapid_key_file": paths::VAPID_KEY_FILE, "contact": p.contact}}),
    );
    let value = json!({
        "server": {
            "bind": format!("0.0.0.0:{}", paths::PORT),
            "cors_origins": cors,
            "docs": spec.server.docs,
        },
        "database": {
            // The migration Job migrates; a pod never does.
            "migrate_on_start": false,
            "max_connections": spec.database.max_connections,
        },
        "oidc": {
            "issuer": spec.oidc.issuer,
            "client_id": spec.oidc.client_id,
            "audiences": (!spec.oidc.audiences.is_empty()).then_some(&spec.oidc.audiences),
            "groups_claim": spec.oidc.groups_claim,
            "trust_unverified_email": spec.oidc.trust_unverified_email.then_some(true),
        },
        "admin": {"oidc_groups": spec.admin.oidc_groups},
        "retention": {
            "default_ttl": spec.retention.default_ttl,
            "max_ttl": spec.retention.max_ttl,
            "allow_forever": spec.retention.allow_forever.then_some(true),
            "audit": spec.retention.audit,
        },
        "attachments": attachments,
        "channels": {"on_removed": spec.channels.on_removed.as_str()},
        "reconcile": {
            "max_removals": spec.reconcile.max_removals,
            "allow_mass_removal": spec.reconcile.allow_mass_removal,
        },
        "sinks": {
            "dead_letter_retention": spec.sinks.dead_letter_retention,
            "smtp": smtp,
        },
        "push": push,
        "telemetry": telemetry,
    });
    let mut base = match table(value) {
        Ok(t) => t,
        Err(e) => {
            problems.push(format!("the instance's file is {e}"));
            return Err(problems);
        }
    };
    if let Err(more) = merge_extra(spec, &mut base) {
        problems.extend(more);
    }
    if problems.is_empty() {
        Ok(base)
    } else {
        Err(problems)
    }
}

/// The variables and files the instance's own spec needs.
fn instance_secret_mounts(input: &Input<'_>) -> (Vec<SecretEnv>, Vec<SecretFile>) {
    let (ns, spec) = (input.namespace, input.spec);
    let names = Names(input.name);
    let mut env = Vec::new();
    let mut files = Vec::new();
    let mut push_env = |name: &str, source: SecretSource, generated: bool| {
        env.push(SecretEnv {
            name: name.into(),
            source,
            generated,
        });
    };
    let db = &spec.database.url_secret_ref;
    push_env(
        "WNK_DATABASE__URL",
        SecretSource::new(ns, &db.name, &db.key),
        false,
    );
    if let Some(s3) = &spec.attachments.s3 {
        let c = &s3.credentials_secret_ref;
        push_env(
            "WNK_ATTACHMENTS__ACCESS_KEY",
            SecretSource::new(ns, &c.name, &c.access_key_id_key),
            false,
        );
        push_env(
            "WNK_ATTACHMENTS__SECRET_KEY",
            SecretSource::new(ns, &c.name, &c.secret_access_key_key),
            false,
        );
    } else if let Some(fs) = &spec.attachments.filesystem {
        match &fs.token_secret_ref {
            Some(r) => push_env(
                "WNK_ATTACHMENTS__TOKEN_SECRET",
                SecretSource::new(ns, &r.name, &r.key),
                false,
            ),
            None => push_env(
                "WNK_ATTACHMENTS__TOKEN_SECRET",
                SecretSource::new(ns, &names.attachments_key(), paths::GENERATED_KEY),
                true,
            ),
        }
    }
    match &spec.sinks.fingerprint_key_secret_ref {
        Some(r) => push_env(
            "WNK_SINKS__FINGERPRINT_KEY",
            SecretSource::new(ns, &r.name, &r.key),
            false,
        ),
        None => push_env(
            "WNK_SINKS__FINGERPRINT_KEY",
            SecretSource::new(ns, &names.fingerprint_key(), paths::GENERATED_KEY),
            true,
        ),
    }
    if let Some(r) = spec
        .smtp
        .as_ref()
        .and_then(|s| s.password_secret_ref.as_ref())
    {
        push_env(
            paths::SMTP_PASSWORD_ENV,
            SecretSource::new(ns, &r.name, &r.key),
            false,
        );
    }
    if let Some(p) = &spec.web_push {
        let r = &p.vapid_key_secret_ref;
        files.push(SecretFile {
            path: paths::VAPID_KEY_FILE.into(),
            source: SecretSource::new(ns, &r.name, &r.key),
        });
    }
    (env, files)
}

/// Where an object's entry goes in the file.
fn section(kind: ObjectKind) -> &'static [&'static str] {
    match kind {
        ObjectKind::ServiceAccount => &["service_accounts"],
        ObjectKind::Channel => &["channels", "managed"],
        ObjectKind::Transform => &["transforms"],
        ObjectKind::TrustPolicy => &["trust_policy"],
    }
}

/// Appends `entry` to the array at `path` of `table`.
fn push_entry(table: &mut toml::Table, path: &[&str], entry: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = table;
    for part in parents {
        let next = current
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let toml::Value::Table(t) = next else {
            return;
        };
        current = t;
    }
    match current
        .entry(last.to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()))
    {
        toml::Value::Array(items) => items.push(entry),
        other => *other = toml::Value::Array(vec![entry]),
    }
}

/// `table` as Notify loads it (no `WNK_*` layer): parsed, then validated.
fn check(table: toml::Table) -> Result<Config, String> {
    let config: Config = toml::Value::Table(table)
        .try_into()
        .map_err(|e| format!("{e}"))?;
    config.validate().map_err(|e| format!("{e:#}"))?;
    Ok(config)
}

/// An object's entry, or why it can't have one (a Secret of its).
fn entry(view: &ObjectView, secrets: &SecretStates) -> Result<Value, (RenderState, String)> {
    let id = view.id();
    let value = match &view.spec {
        ObjectSpec::ServiceAccount(s) => json!({
            "id": id,
            "display_name": s.display_name,
            "description": s.description,
        }),
        ObjectSpec::Channel(c) => {
            let mut sinks = Vec::new();
            for sink in &c.sinks {
                let secret_ref = match &sink.secret_ref {
                    None => None,
                    Some(r) => {
                        let source = SecretSource::new(&view.namespace, &r.name, &r.key);
                        match secrets.get(&source) {
                            Some(SecretState::Missing(m)) => {
                                return Err((RenderState::SecretMissing, m.clone()));
                            }
                            Some(SecretState::NotAllowed(m)) => {
                                return Err((RenderState::SecretNotAllowed, m.clone()));
                            }
                            Some(SecretState::Present) | None => Some(format!(
                                "file:{}/{}",
                                paths::SINK_SECRETS_DIR,
                                sink_entry(&source)
                            )),
                        }
                    }
                };
                sinks.push(json!({
                    "type": sink.r#type.as_str(),
                    "url": sink.url,
                    "address": sink.address,
                    "secret_ref": secret_ref,
                }));
            }
            let access: Vec<Value> = c
                .access
                .iter()
                .map(|a| {
                    json!({
                        "oidc_group": a.oidc_group,
                        "oidc_sub": a.oidc_sub,
                        "email": a.email,
                        "service_account": a.service_account,
                        "scopes": a.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let publishers: Vec<Value> = c
                .publishers
                .iter()
                .map(|p| {
                    json!({
                        "issuer": p.issuer,
                        "subject": p.subject,
                        "scopes": p.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let feeds: Vec<Value> = c
                .feeds
                .iter()
                .map(|f| {
                    json!({
                        "url": f.url,
                        "publisher": f.publisher,
                        "every": f.every,
                        "tags": f.tags,
                        "max_per_poll": f.max_per_poll,
                        "allow_private_network": f.allow_private_network,
                        "lang": f.lang.as_str(),
                    })
                })
                .collect();
            json!({
                "id": id,
                "name": c.name,
                "default_ttl": c.default_ttl,
                "max_ttl": c.max_ttl,
                "close_after": c.close_after,
                "default_priority": c.default_priority.map(|p| p.as_str()),
                "receipts": c.receipts,
                "limits": c.publish_per_min.map(|n| json!({"publish_per_min": n})),
                "access": access,
                "publishers": publishers,
                "sinks": sinks,
                "feeds": feeds,
            })
        }
        ObjectSpec::Transform(t) => json!({
            "id": id,
            "channels": t.channels,
            "lang": t.lang.as_str(),
            "split": t.split,
            "max_items": t.max_items,
            "title": t.title,
            "title_fallback": t.title_fallback,
            "body": t.body,
            "link": t.link,
            "external_id": t.external_id,
            "group_key": t.group_key,
            "tags": t.tags,
            "priority": t.priority,
            "status": t.status,
            "state": t.state,
            "ack": t.ack,
            "update": t.update,
            "raw": t.raw.as_str(),
        }),
        ObjectSpec::TrustPolicy(p) => json!({
            "issuer": p.issuer,
            "subject": p.subject,
            "channels": p.channels,
            "scopes": p.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        }),
    };
    Ok(prune(value))
}

/// The channel ids an object publishes to (a transform, a trust policy).
fn channels_named(view: &ObjectView) -> &[String] {
    match &view.spec {
        ObjectSpec::Transform(t) => &t.channels,
        ObjectSpec::TrustPolicy(p) => &p.channels,
        _ => &[],
    }
}

/// The boot hash of a file Notify loaded: what a reload doesn't adopt
/// (`reloaded_from` over an empty file keeps only that), and the
/// federation issuers.
pub fn boot_hash(config: &Config) -> Result<String, RenderError> {
    let boot = config.reloaded_from(Config::default());
    let mut issuers = config.federation_issuers();
    issuers.sort();
    let bytes = serde_json::to_vec(&(&boot, &issuers))
        .map_err(|e| RenderError::Invalid(format!("the boot configuration: {e}")))?;
    Ok(content_hash(&bytes))
}

/// The instance's file, from its spec and its objects.
pub fn render(input: &Input<'_>) -> Result<Rendered, RenderError> {
    let base = instance_table(input).map_err(|p| RenderError::Invalid(p.join("; ")))?;
    check(base.clone()).map_err(|e| {
        RenderError::Invalid(format!(
            "the instance's own configuration Notify would refuse: {e}"
        ))
    })?;
    let (env, mut files) = instance_secret_mounts(input);

    let mut views: Vec<ObjectView> = input
        .objects
        .iter()
        .filter(|v| !v.deleting)
        .cloned()
        .collect();
    objects::sort(&mut views);
    let lost = objects::conflicts(&views);

    let mut full = base.clone();
    let mut accounts: Vec<toml::Value> = Vec::new();
    let mut managed_channels: BTreeSet<String> = BTreeSet::new();
    let mut renders = BTreeMap::new();
    let mut sink_files = BTreeSet::new();
    for view in &views {
        let key = view.key();
        let mut render = ObjectRender {
            id: view.id(),
            state: RenderState::Rendered,
            message: None,
            conflict_with: None,
            unmanaged_channels: Vec::new(),
            generation: view.generation,
            applied_generation: input.previous.get(&key).and_then(|r| r.applied_generation),
        };
        if let Some(winner) = lost.get(&key) {
            render.state = RenderState::Conflict;
            render.conflict_with = Some(winner.clone());
            render.message = Some(format!(
                "the older {} {winner} has the id {:?} on this instance",
                view.kind().noun(),
                render.id.as_deref().unwrap_or_default()
            ));
            renders.insert(key, render);
            continue;
        }
        let checked = entry(view, input.secrets).and_then(|value| {
            let toml_entry = toml::Value::try_from(&value)
                .map_err(|e| (RenderState::Invalid, format!("not TOML: {e}")))?;
            // Alone in the instance's file, with the service accounts a
            // channel's feeds may name.
            let mut alone = base.clone();
            for account in &accounts {
                push_entry(
                    &mut alone,
                    section(ObjectKind::ServiceAccount),
                    account.clone(),
                );
            }
            push_entry(&mut alone, section(view.kind()), toml_entry.clone());
            check(alone).map_err(|e| (RenderState::Invalid, e))?;
            Ok(toml_entry)
        });
        match checked {
            Ok(toml_entry) => {
                match view.kind() {
                    ObjectKind::ServiceAccount => accounts.push(toml_entry.clone()),
                    ObjectKind::Channel => {
                        managed_channels.extend(render.id.clone());
                        if let ObjectSpec::Channel(c) = &view.spec {
                            for r in c.sinks.iter().filter_map(|s| s.secret_ref.as_ref()) {
                                sink_files.insert(SecretSource::new(
                                    &view.namespace,
                                    &r.name,
                                    &r.key,
                                ));
                            }
                        }
                    }
                    ObjectKind::Transform | ObjectKind::TrustPolicy => {
                        render.unmanaged_channels = channels_named(view)
                            .iter()
                            .filter(|c| !managed_channels.contains(*c))
                            .cloned()
                            .collect();
                    }
                }
                push_entry(&mut full, section(view.kind()), toml_entry);
            }
            Err((state, message)) => {
                render.state = state;
                render.message = Some(message);
            }
        }
        renders.insert(key, render);
    }
    files.extend(sink_files.into_iter().map(|source| SecretFile {
        path: format!("{}/{}", paths::SINK_SECRETS_DIR, sink_entry(&source)),
        source,
    }));

    let text = toml::to_string(&full)
        .map_err(|e| RenderError::Invalid(format!("the configuration is not TOML: {e}")))?;
    // What Notify reads, read as Notify does.
    let config = Config::from_toml_str(&text).map_err(|e| {
        RenderError::Invalid(format!("the configuration Notify would refuse: {e:#}"))
    })?;
    Ok(Rendered {
        boot_hash: boot_hash(&config)?,
        config_digest: content_hash(text.as_bytes()),
        config,
        text,
        env,
        files,
        objects: renders,
    })
}

#[cfg(test)]
mod tests {
    use api::local_instance::LocalInstanceSpec;
    use serde_json::json;

    use super::*;

    fn spec(value: Value) -> LocalInstanceSpec {
        serde_json::from_value(value).unwrap()
    }

    fn instance() -> LocalInstanceSpec {
        spec(json!({
            "database": {"urlSecretRef": {"name": "db", "key": "url"}, "maxConnections": 20},
            "attachments": {"s3": {"endpoint": "http://rustfs:9000", "bucket": "notify",
                                   "credentialsSecretRef": {"name": "s3"}}},
            "oidc": {"issuer": "https://id.example", "clientId": "notify"},
            "admin": {"oidcGroups": ["notify-admins"]},
            "smtp": {"url": "smtps://relay.example:465", "from": "notify@example.com",
                     "username": "notify", "passwordSecretRef": {"name": "smtp", "key": "password"}},
            "webPush": {"vapidKeySecretRef": {"name": "vapid", "key": "key.pem"},
                        "contact": "mailto:ops@example.com"},
            "telemetry": {"otlpEndpoint": "http://collector:4318", "sampleRatio": "0.5"},
            "retention": {"defaultTtl": "30d"},
            "ingress": {"api": {"host": "notify.example.com"}, "web": {"host": "app.example.com"}},
        }))
    }

    fn view(kind: &str, ns: &str, name: &str, created: &str, spec: Value) -> ObjectView {
        let spec = match kind {
            "sa" => ObjectSpec::ServiceAccount(serde_json::from_value(spec).unwrap()),
            "ch" => ObjectSpec::Channel(serde_json::from_value(spec).unwrap()),
            "tr" => ObjectSpec::Transform(serde_json::from_value(spec).unwrap()),
            _ => ObjectSpec::TrustPolicy(serde_json::from_value(spec).unwrap()),
        };
        ObjectView {
            namespace: ns.into(),
            name: name.into(),
            created: Some(created.parse().unwrap()),
            generation: Some(1),
            spec,
            deleting: false,
        }
    }

    fn objects() -> Vec<ObjectView> {
        vec![
            view(
                "ch",
                "team",
                "alerts",
                "2026-01-02T00:00:00Z",
                json!({
                    "name": "Alerts",
                    "defaultTtl": "48h",
                    "closeAfter": "0",
                    "defaultPriority": "high",
                    "publishPerMin": 120,
                    "access": [
                        {"oidcGroup": "ops", "scopes": ["subscribe", "manage"]},
                        {"serviceAccount": "rss", "scopes": ["publish"]},
                    ],
                    "publishers": [{"issuer": "https://token.actions.githubusercontent.com",
                                    "subject": "repo:acme/app:ref:refs/heads/main",
                                    "scopes": ["publish"]}],
                    "sinks": [
                        {"type": "webhook", "url": "https://hooks.example.com/notify",
                         "secretRef": {"name": "hook", "key": "secret"}},
                        {"type": "email", "address": "oncall@example.com"},
                    ],
                    "feeds": [{"url": "https://blog.example.com/feed.xml", "publisher": "rss",
                               "every": "1h", "tags": ["blog"]}],
                }),
            ),
            view(
                "sa",
                "team",
                "rss",
                "2026-01-01T00:00:00Z",
                json!({"displayName": "RSS poller"}),
            ),
            view(
                "tr",
                "team",
                "grafana",
                "2026-01-03T00:00:00Z",
                json!({"channels": ["alerts", "dynamic"], "title": "body.title",
                       "body": "body.message", "split": "body.alerts", "maxItems": 10,
                       "raw": "item"}),
            ),
            view(
                "tp",
                "team",
                "ci",
                "2026-01-04T00:00:00Z",
                json!({"issuer": "https://token.actions.githubusercontent.com",
                       "subject": "repo:acme/app:ref:refs/heads/main",
                       "channels": ["alerts"], "scopes": ["publish"]}),
            ),
        ]
    }

    fn render_with(
        spec: &LocalInstanceSpec,
        objects: &[ObjectView],
        secrets: &SecretStates,
    ) -> Result<Rendered, RenderError> {
        render(&Input {
            namespace: "notify",
            name: "main",
            spec,
            objects,
            secrets,
            previous: &BTreeMap::new(),
        })
    }

    #[test]
    fn an_instance_and_one_of_each_object_round_trip_through_notify() {
        let spec = instance();
        let rendered = render_with(&spec, &objects(), &SecretStates::new()).unwrap();
        for (key, render) in &rendered.objects {
            assert_eq!(render.state, RenderState::Rendered, "{key}: {render:?}");
        }
        // Notify reads back what the operator wrote, and validates it.
        let config = Config::from_toml_str(&rendered.text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.channels.managed.len(), 1);
        assert_eq!(config.channels.managed[0].id, "alerts");
        assert_eq!(config.channels.managed[0].feeds.len(), 1);
        assert_eq!(config.service_accounts[0].id, "rss");
        assert_eq!(config.transforms[0].id, "grafana");
        assert_eq!(config.trust_policy.len(), 1);
        assert!(!config.database.migrate_on_start);
        assert_eq!(config.database.max_connections, 20);
        assert_eq!(config.admin.oidc_groups, ["notify-admins"]);
        assert_eq!(config.server.cors_origins, ["https://app.example.com"]);
        assert_eq!(
            config.telemetry.as_ref().map(|t| t.service_name.as_str()),
            Some("main")
        );
        // No secret in the file: references only.
        let sink = &config.channels.managed[0].sinks[0];
        let reference = sink.secret_ref.as_deref().unwrap();
        assert!(
            reference.starts_with("file:/var/run/notify/sinks/sink-"),
            "{reference}"
        );
        let smtp = config.sinks.smtp.as_ref().unwrap();
        assert_eq!(smtp.password_ref.as_deref(), Some("env:WNK_SMTP_PASSWORD"));
        for absent in [
            "postgres://",
            "access_key",
            "secret_key",
            "token_secret",
            "fingerprint_key",
        ] {
            assert!(
                !rendered.text.contains(absent),
                "{absent} in\n{}",
                rendered.text
            );
        }
        // The pods get the secrets.
        let env: Vec<_> = rendered.env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            env,
            [
                "WNK_DATABASE__URL",
                "WNK_ATTACHMENTS__ACCESS_KEY",
                "WNK_ATTACHMENTS__SECRET_KEY",
                "WNK_SINKS__FINGERPRINT_KEY",
                "WNK_SMTP_PASSWORD",
            ]
        );
        assert!(rendered.env[3].generated);
        assert_eq!(rendered.env[3].source.name, "main-fingerprint-key");
        let files: Vec<_> = rendered.files.iter().map(|f| &f.source.name).collect();
        assert_eq!(files, ["vapid", "hook"]);
        assert_eq!(rendered.files[1].source.namespace, "team");
        assert_eq!(
            rendered.files[1].path,
            reference.trim_start_matches("file:")
        );
        // The transform names a channel no NotifyChannel declares.
        let transform = &rendered.objects["NotifyTransform/team/grafana"];
        assert_eq!(transform.unmanaged_channels, ["dynamic"]);
        assert!(
            rendered.objects["NotifyTrustPolicy/team/ci"]
                .unmanaged_channels
                .is_empty()
        );
        // Deterministic: the order the objects come in doesn't matter.
        let mut reversed = objects();
        reversed.reverse();
        let again = render_with(&spec, &reversed, &SecretStates::new()).unwrap();
        assert_eq!(again.text, rendered.text);
        assert_eq!(again.config_digest, rendered.config_digest);
    }

    #[test]
    fn the_boot_hash_moves_with_what_notify_reads_at_boot_only() {
        let spec = instance();
        let base = render_with(&spec, &objects(), &SecretStates::new()).unwrap();

        // A channel changed: hot, the file changes, the boot hash doesn't.
        let mut changed = objects();
        if let ObjectSpec::Channel(c) = &mut changed[0].spec {
            c.name = Some("Alerts!".into());
            c.default_ttl = Some("72h".into());
        }
        let hot = render_with(&spec, &changed, &SecretStates::new()).unwrap();
        assert_ne!(hot.config_digest, base.config_digest);
        assert_eq!(hot.boot_hash, base.boot_hash);
        assert!(base.config.unreloadable(&hot.config).is_empty());

        // The platform admins: read at boot.
        let mut admins = instance();
        admins.admin.oidc_groups.push("more-admins".into());
        let boot = render_with(&admins, &objects(), &SecretStates::new()).unwrap();
        assert_ne!(boot.boot_hash, base.boot_hash);
        assert!(!base.config.unreloadable(&boot.config).is_empty());

        // A trust policy of a new issuer: its verifier is built at boot.
        let mut issuer = objects();
        if let ObjectSpec::TrustPolicy(p) = &mut issuer[3].spec {
            p.issuer = "https://gitlab.example.com".into();
        }
        let new_issuer = render_with(&spec, &issuer, &SecretStates::new()).unwrap();
        assert_ne!(new_issuer.boot_hash, base.boot_hash);
        assert!(!base.config.unreloadable(&new_issuer.config).is_empty());
    }

    #[test]
    fn an_id_held_by_an_older_object_is_a_conflict() {
        let mut views = objects();
        views.push(view(
            "sa",
            "other",
            "bot",
            "2026-02-01T00:00:00Z",
            json!({"id": "rss"}),
        ));
        let rendered = render_with(&instance(), &views, &SecretStates::new()).unwrap();
        let lost = &rendered.objects["NotifyServiceAccount/other/bot"];
        assert_eq!(lost.state, RenderState::Conflict);
        assert_eq!(lost.conflict_with.as_deref(), Some("team/rss"));
        assert_eq!(rendered.config.service_accounts.len(), 1);
    }

    #[test]
    fn an_object_notify_would_refuse_is_left_out_alone() {
        let mut views = objects();
        // Its feed's publisher holds no `publish` grant.
        if let ObjectSpec::Channel(c) = &mut views[0].spec {
            c.access.retain(|a| a.service_account.is_none());
        }
        views.push(view(
            "tr",
            "team",
            "broken",
            "2026-01-05T00:00:00Z",
            json!({"channels": ["alerts"], "title": "body.("}),
        ));
        let rendered = render_with(&instance(), &views, &SecretStates::new()).unwrap();
        let channel = &rendered.objects["NotifyChannel/team/alerts"];
        assert_eq!(channel.state, RenderState::Invalid);
        assert!(
            channel.message.as_deref().unwrap().contains("publish"),
            "{channel:?}"
        );
        assert_eq!(
            rendered.objects["NotifyTransform/team/broken"].state,
            RenderState::Invalid
        );
        assert!(rendered.config.channels.managed.is_empty());
        assert_eq!(rendered.config.transforms.len(), 1);
        // The channel left out: the transform's channels are now dynamic.
        assert_eq!(
            rendered.objects["NotifyTransform/team/grafana"].unmanaged_channels,
            ["alerts", "dynamic"]
        );
    }

    #[test]
    fn a_sink_secret_missing_or_not_labelled_leaves_its_channel_out() {
        let views = objects();
        let source = object_secrets(&views).pop().unwrap();
        assert_eq!(source, SecretSource::new("team", "hook", "secret"));
        for (state, expected) in [
            (
                SecretState::Missing("gone".into()),
                RenderState::SecretMissing,
            ),
            (
                SecretState::NotAllowed("no label".into()),
                RenderState::SecretNotAllowed,
            ),
        ] {
            let secrets = SecretStates::from([(source.clone(), state)]);
            let rendered = render_with(&instance(), &views, &secrets).unwrap();
            assert_eq!(
                rendered.objects["NotifyChannel/team/alerts"].state,
                expected
            );
            assert!(rendered.files.iter().all(|f| f.source.name != "hook"));
        }
    }

    #[test]
    fn extra_config_fills_only_what_nothing_else_renders() {
        let mut s = instance();
        s.extra_config = serde_json::from_value(json!({"devices": {"max_per_user": 5}})).unwrap();
        let rendered = render_with(&s, &[], &SecretStates::new()).unwrap();
        assert_eq!(rendered.config.devices.max_per_user, 5);
        for refused in [
            json!({"channels": {"managed": [{"id": "x"}]}}),
            json!({"service_accounts": [{"id": "x"}]}),
            json!({"database": {"url": "postgres://u:p@db/notify"}}),
            json!({"oidc": {"issuer": "https://other"}}),
            json!({"devices": {"max_per_user": 0}}),
        ] {
            s.extra_config = serde_json::from_value(refused.clone()).unwrap();
            assert!(
                render_with(&s, &[], &SecretStates::new()).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn deleted_objects_leave_the_file() {
        let mut views = objects();
        for v in &mut views {
            v.deleting = true;
        }
        let rendered = render_with(&instance(), &views, &SecretStates::new()).unwrap();
        assert!(rendered.objects.is_empty());
        assert!(rendered.config.channels.managed.is_empty());
        assert!(object_secrets(&views).is_empty());
    }
}
