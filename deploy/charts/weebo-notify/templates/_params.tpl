{{/*
This operator's parameters for the kit's library chart (weebo-operator-lib,
see its README): everything templates/operator.yaml renders that is not a
value.
*/}}
{{- define "operator.params" -}}
{{- /* What the operator reads and writes in the namespaces of its instances
(operator.namespacedRules): cluster-wide in the ClusterRole, or — with
watch.namespace — only in that namespace, through the kit's watch Role
(params.watchRules), which a watch.clusterKinds: false release renders
too. */ -}}
{{- $watchNamespace := include "weebo-operator.watchNamespace" . -}}
group: notify.weebo.io
versions: [v1alpha1]
# Every kind the operator watches (ClusterRole).
kinds:
  - notifylocalinstances
  - notifychannels
  - notifyserviceaccounts
  - notifytransforms
  - notifytrustpolicies
  - notifyapikeys
# The namespaced kinds — all of them, the operator has no cluster-scoped
# kind: what a team may manage (rbac.namespacedExample), and the watch
# Role's (watch.namespace).
namespacedKinds:
  - notifylocalinstances
  - notifychannels
  - notifyserviceaccounts
  - notifytransforms
  - notifytrustpolicies
  - notifyapikeys
# The operator checks the Secrets the instances and their objects name, in
# every namespace that has one, and writes the keys it generates (the
# sinks' fingerprint key…), the sinks' secrets copied next to the pods and
# the API keys' Secrets (NotifyApiKey secretTargets):
# said explicitly, also limited to one namespace.
clusterSecrets: true
# Kinds the admission webhook checks (`/validate`), on CREATE/UPDATE: a
# configuration Notify's own model refuses (a transform's CEL that doesn't
# compile…), pods the operator doesn't run, a tag the version guard
# refuses, a Secret that doesn't opt in.
admitted:
  - notifylocalinstances
  - notifychannels
  - notifyserviceaccounts
  - notifytransforms
  - notifytrustpolicies
  - notifyapikeys
# Cluster-wide only: the namespaced rules below, and the labels of the
# namespaces objects come from (objectSelector.namespaces.from: Selector).
# Limited to one namespace, none of them: they go to watchRules. No Secret
# watch.
clusterRules:
  {{- if not $watchNamespace }}
  - apiGroups: [""]
    resources: [namespaces]
    verbs: [get, list, watch]
  {{- include "operator.namespacedRules" . | nindent 2 }}
  {{- end }}
# Limited to one namespace (watch.namespace): the same rules, in the watch
# Role of that namespace only.
watchRules:
  {{- if $watchNamespace }}
  {{- include "operator.namespacedRules" . | nindent 2 }}
  {{- end }}
# The operator's own environment variables.
env:
  - name: WEEBO_NOTIFY_ALLOWED_IMAGES
    value: {{ join "," .Values.managedInstances.allowedImages | quote }}
  # Where NotifyApiKeys write their keys (Kubernetes Secrets, Vault).
  - name: WEEBO_NOTIFY_SECRET_STORE
    value: {{ toJson .Values.secretStore | quote }}
  {{- with .Values.defaults.image }}
  - name: WEEBO_NOTIFY_DEFAULT_IMAGE
    value: {{ . | quote }}
  {{- end }}
egress:
  - comment: The instances' pods, whose /healthz the operator reads.
    rules: {{ toJson .Values.networkPolicy.egress.notify }}
# Alerts on the operator's own metrics, appended to the kit's.
alerts: []
{{- end -}}

{{/* The rules on the namespaced resources of the instances' namespaces:
what an instance is made of (server-side apply, each object read first to
never take over someone else's): its Deployments, Services, account,
claim, configuration ConfigMap, PodDisruptionBudget, NetworkPolicy,
migration Jobs, Ingresses and ServiceMonitor; and its pods (their status,
the configuration each one runs). No pods/exec: the operator never signals
a pod. */}}
{{- define "operator.namespacedRules" -}}
- apiGroups: [apps]
  resources: [deployments]
  verbs: [get, list, watch, create, patch, delete]
- apiGroups: [""]
  resources: [services, serviceaccounts, persistentvolumeclaims, configmaps]
  verbs: [get, list, watch, create, patch, delete]
- apiGroups: [policy]
  resources: [poddisruptionbudgets]
  verbs: [get, list, watch, create, patch, delete]
- apiGroups: [networking.k8s.io]
  resources: [networkpolicies, ingresses]
  verbs: [get, list, watch, create, patch, delete]
- apiGroups: [batch]
  resources: [jobs]
  verbs: [get, list, watch, create, patch, delete]
- apiGroups: [monitoring.coreos.com]
  resources: [servicemonitors]
  verbs: [get, list, watch, create, patch, delete]
- apiGroups: [""]
  resources: [pods]
  verbs: [get, list, watch]
{{- end -}}
