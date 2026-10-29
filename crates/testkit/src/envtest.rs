//! Real `kube-apiserver` + `etcd` (the `envtest` crate, as the kit's
//! `EnvTestCluster`) with this operator's CRDs from `deploy/crd/`
//! installed.
//!
//! Building it needs Go and `libclang.so` — see `task doctor` and the
//! Taskfile's `env:` block. The control plane: with `KUBEBUILDER_ASSETS`
//! set (a directory holding `kube-apiserver`, `etcd`, `kubectl`), those
//! binaries, nothing downloaded; unset, the kit's default — the latest
//! stable release, fetched into setup-envtest's cache when missing.

use std::path::Path;

use ::envtest::Environment;
use kube::Client;

/// A control plane, stopped when dropped (even by a failing test).
pub struct Cluster {
    server: ::envtest::Server,
    client: Client,
}

impl Cluster {
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// The admin kubeconfig.
    pub fn kubeconfig(&self) -> kube::config::Kubeconfig {
        #[expect(clippy::expect_used, reason = "a test harness fails loudly")]
        self.server.kubeconfig().expect("envtest kubeconfig")
    }
}

/// Starts a control plane with every CRD of `deploy/crd/` installed
/// (`task recu` regenerates them).
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a broken harness fails loudly at the call site"
)]
pub async fn start() -> Cluster {
    // `ring` and `aws-lc-rs` are both linked; rustls needs one picked. Err:
    // another test installed it first, which is as good.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let crd_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/crd");
    let crd_dir = crd_dir
        .canonicalize()
        .unwrap_or_else(|e| panic!("{}: {e} — generate the CRDs first", crd_dir.display()));
    let mut environment = Environment::default()
        .with_crds_from_paths(vec![crd_dir.display().to_string()])
        .expect("the CRD directory must be readable");
    if let Some(assets) = std::env::var("KUBEBUILDER_ASSETS")
        .ok()
        .filter(|a| !a.is_empty())
    {
        environment.binary_assets_settings.download_binary_assets = false;
        environment.binary_assets_settings.binary_assets_directory = Some(assets);
    }
    let server = environment
        .create()
        .await
        .expect("envtest control plane failed to start");
    let client = server.client().expect("envtest client");
    Cluster { server, client }
}
