//! [`ObjectDirectory`] on the Kubernetes API: the `NotifyChannel`s,
//! `NotifyServiceAccount`s, `NotifyTransform`s and `NotifyTrustPolicy`s in
//! the operator's scope recording an instance in `status.instance` (their
//! own controller binds them), kept only when the instance still accepts
//! them — its `objectSelector.selector` matches their labels, and they are
//! in its namespace or one its `objectSelector.namespaces` accepts (a
//! consent withdrawn after the binding takes them out).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use api::local_instance::ObjectSelector;
use api::{Channel, ServiceAccount, Transform, TrustPolicy};
use application::objects::{ObjectSpec, ObjectView};
use application::ports::{ApiError, ObjectDirectory, Owner};
use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, ListParams};
use kube::core::{Selector, SelectorExt};
use kube::{Client, ResourceExt};
use weebo_kit_adapters::instance::STORE_READY_TIMEOUT;
use weebo_kit_api::BoundInstance;
use weebo_kit_application::sharing::shares_with;
use weebo_kit_runtime::shared::SharedWatches;

pub struct KubeObjects {
    client: Client,
    watched_namespace: Option<String>,
    selector: Option<Selector>,
    /// The binary's shared watches: each kind read from its store once
    /// listed, from the API otherwise.
    watches: SharedWatches,
    /// The labels of the namespaces read lately (no watch on Namespaces):
    /// one GET per namespace per [`NAMESPACE_LABELS_TTL`], not per pass.
    namespace_labels: Mutex<HashMap<String, (Instant, Labels)>>,
}

/// A namespace's labels (`None`: it doesn't exist).
type Labels = Option<BTreeMap<String, String>>;

/// How long a namespace's labels are trusted: a consent withdrawn takes
/// at most this long to be seen.
const NAMESPACE_LABELS_TTL: Duration = Duration::from_secs(30);
/// At most this many namespaces' labels kept.
const NAMESPACE_LABELS_CAP: usize = 1024;

/// What the directory needs of a kind bound to an instance.
trait Bound:
    kube::Resource<DynamicType = (), Scope = k8s_openapi::NamespaceResourceScope>
    + Clone
    + std::fmt::Debug
    + serde::de::DeserializeOwned
    + Send
    + Sync
    + 'static
{
    fn bound(&self) -> Option<&BoundInstance>;
    fn object_spec(&self) -> ObjectSpec;
}

macro_rules! bound {
    ($kind:ty, $variant:ident) => {
        impl Bound for $kind {
            fn bound(&self) -> Option<&BoundInstance> {
                self.status.as_ref()?.instance.as_ref()
            }
            fn object_spec(&self) -> ObjectSpec {
                ObjectSpec::$variant(self.spec.clone())
            }
        }
    };
}

bound!(Channel, Channel);
bound!(ServiceAccount, ServiceAccount);
bound!(Transform, Transform);
bound!(TrustPolicy, TrustPolicy);

fn view<K: Bound>(object: K) -> ObjectView {
    ObjectView {
        namespace: object.namespace().unwrap_or_default(),
        name: object.name_any(),
        created: object.meta().creation_timestamp.as_ref().map(|t| t.0),
        generation: object.meta().generation,
        spec: object.object_spec(),
        deleting: object.meta().deletion_timestamp.is_some(),
    }
}

impl KubeObjects {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            watched_namespace: None,
            selector: None,
            watches: SharedWatches::default(),
            namespace_labels: Mutex::default(),
        }
    }

    /// Read the objects from the binary's shared watches (on the scope of
    /// [`Self::with_scope`]) instead of a LIST on every instance pass.
    pub fn with_watches(mut self, watches: SharedWatches) -> Self {
        self.watches = watches;
        self
    }

    /// The operator's watch scope.
    pub fn with_scope(
        mut self,
        watched_namespace: Option<String>,
        selector: Option<Selector>,
    ) -> Self {
        self.watched_namespace = watched_namespace;
        self.selector = selector;
        self
    }

    /// Every `K` in the operator's scope that `keep` keeps: from its
    /// shared watch once it has listed (an unsynced store is never read as
    /// empty; only the kept objects are cloned), from the API otherwise.
    async fn list<K: Bound>(&self, keep: impl Fn(&K) -> bool) -> Result<Vec<K>, ApiError> {
        if let Some(shared) = self.watches.get::<K>() {
            let store = shared.store();
            if weebo_kit_adapters::wait_synced(&store, STORE_READY_TIMEOUT).await {
                return Ok(store
                    .state()
                    .iter()
                    .filter(|o| keep(o))
                    .map(|o| K::clone(o))
                    .collect());
            }
        }
        let api: Api<K> = match &self.watched_namespace {
            Some(ns) => Api::namespaced(self.client.clone(), ns),
            None => Api::all(self.client.clone()),
        };
        let params = match &self.selector {
            Some(s) => ListParams::default().labels_from(s),
            None => ListParams::default(),
        };
        api.list(&params)
            .await
            .map(|l| l.items.into_iter().filter(|o| keep(o)).collect())
            .map_err(crate::api_error(format_args!("listing {}", K::kind(&()))))
    }

    /// Those of `K` bound to `instance` that it still accepts.
    async fn bound<K: Bound>(
        &self,
        instance: &Owner,
        own: Option<&Selector>,
        selector: &ObjectSelector,
    ) -> Result<Vec<ObjectView>, ApiError> {
        let bound = BoundInstance::local(instance.namespace.clone(), instance.name.clone());
        let listed = self.list::<K>(|o| o.bound() == Some(&bound)).await?;
        let shared = selector.namespaces.shared_with();
        let mut out = Vec::new();
        for object in listed {
            let namespace = object.namespace().unwrap_or_default();
            if let Some(own) = own
                && !own.matches(object.labels())
            {
                continue;
            }
            if namespace != instance.namespace {
                let labels = self.labels_of(&namespace).await?;
                if !shares_with(shared.as_ref(), Some(&namespace), labels.as_ref()) {
                    continue;
                }
            }
            out.push(view(object));
        }
        Ok(out)
    }

    /// The labels of `namespace` (`None`: it doesn't exist), from the
    /// cache while fresh.
    async fn labels_of(&self, namespace: &str) -> Result<Labels, ApiError> {
        let cache = || {
            self.namespace_labels
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        if let Some((at, labels)) = cache().get(namespace)
            && at.elapsed() < NAMESPACE_LABELS_TTL
        {
            return Ok(labels.clone());
        }
        let labels = Api::<Namespace>::all(self.client.clone())
            .get_opt(namespace)
            .await
            .map_err(crate::api_error(format_args!(
                "reading namespace {namespace}"
            )))?
            .map(|n| n.labels().clone());
        let mut cache = cache();
        if cache.len() >= NAMESPACE_LABELS_CAP {
            cache.retain(|_, (at, _)| at.elapsed() < NAMESPACE_LABELS_TTL);
            if cache.len() >= NAMESPACE_LABELS_CAP {
                cache.clear();
            }
        }
        cache.insert(namespace.to_owned(), (Instant::now(), labels.clone()));
        Ok(labels)
    }
}

#[async_trait::async_trait]
impl ObjectDirectory for KubeObjects {
    async fn bound_to(
        &self,
        instance: &Owner,
        selector: &ObjectSelector,
    ) -> Result<Vec<ObjectView>, ApiError> {
        let own: Option<Selector> = selector
            .selector
            .clone()
            .map(Selector::try_from)
            .transpose()
            .map_err(|e| ApiError::Invalid(format!("objectSelector.selector: {e}")))?;
        let own = own.as_ref();
        let mut views = self
            .bound::<ServiceAccount>(instance, own, selector)
            .await?;
        views.extend(self.bound::<Channel>(instance, own, selector).await?);
        views.extend(self.bound::<Transform>(instance, own, selector).await?);
        views.extend(self.bound::<TrustPolicy>(instance, own, selector).await?);
        Ok(views)
    }
}
