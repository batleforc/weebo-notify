// Input of `weebo-crd-docs` (@weebo/operator-docs), run by `task recu` /
// `npm run gen:crds`: what the CRD reference pages say beyond the CRDs
// themselves. Paths default to the expected tree (../deploy/crd →
// content/docs/crds + public/crd-schemas).
import { defineConfig } from "@weebo/operator-docs/crd-docs";

const install = ["Install via Helm", "/docs/guides/install"];

export default defineConfig({
  // `NotifyLocalInstance` → example named `my-local-instance`.
  kindPrefix: "Notify",
  // Sidebar order follows the order you'd create things in, not the alphabet.
  order: [
    "NotifyLocalInstance",
    "NotifyChannel",
    "NotifyServiceAccount",
    "NotifyTransform",
    "NotifyTrustPolicy",
  ],
  // The guide that explains each kind, linked from its reference page.
  guides: {
    NotifyLocalInstance: ["Migrate from the Helm chart", "/docs/guides/migrate-from-helm-chart"],
    NotifyChannel: install,
    NotifyServiceAccount: install,
    NotifyTransform: install,
    NotifyTrustPolicy: install,
  },
  // A minimal, realistic `spec` per kind, checked against the schema (every
  // required field present, every key a real field): a renamed field fails
  // generation instead of shipping a stale example.
  examples: {
    NotifyLocalInstance: {
      database: { urlSecretRef: { name: "notify-db", key: "url" } },
      attachments: {
        s3: {
          endpoint: "https://s3.example.com",
          bucket: "notify",
          credentialsSecretRef: { name: "notify-s3" },
        },
      },
      oidc: { issuer: "https://id.example.com", clientId: "notify" },
      webPush: {
        vapidKeySecretRef: { name: "notify-vapid", key: "key.pem" },
        contact: "mailto:ops@example.com",
      },
      ingress: {
        className: "nginx",
        api: { host: "notify.example.com", tlsSecretName: "notify-tls" },
        web: { host: "app.example.com" },
      },
    },
    NotifyChannel: {
      name: "Alerts",
      defaultTtl: "48h",
      access: [
        { oidcGroup: "ops", scopes: ["subscribe", "manage"] },
        { serviceAccount: "grafana", scopes: ["publish"] },
      ],
      sinks: [{ type: "email", address: "oncall@example.com" }],
    },
    NotifyServiceAccount: { displayName: "Grafana alerting" },
    NotifyTransform: {
      channels: ["alerts"],
      title: "body.title",
      body: "body.message",
      split: "body.alerts",
      maxItems: 10,
    },
    NotifyTrustPolicy: {
      issuer: "https://token.actions.githubusercontent.com",
      subject: "repo:acme/app:ref:refs/heads/main",
      channels: ["alerts"],
      scopes: ["publish"],
    },
  },
  // The operator reference pages (content/docs/reference/), generated from
  // the Rust sources: reason codes (crates/domain/src/error.rs), Events,
  // metrics & health.
  reference: {
    metricsPrefix: "weebo_notify",
  },
});
