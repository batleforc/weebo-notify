import { OperatorHome } from '@weebo/operator-docs/site';
import { site } from '@/lib/shared';

export default function HomePage() {
  return (
    <OperatorHome
      site={site}
      label="Kubernetes operator · weebo-notify"
      caption="Run weebo-notify, the self-hosted notification hub, from Kubernetes: its instances, then their channels, service accounts, transforms and trust policies — its configuration rendered, checked by Notify's own parser and confirmed loaded."
      actions={[
        { href: '/docs/guides/install', label: 'Get started', primary: true },
        { href: '/docs', label: 'Read the docs' },
        { href: '/docs/ide', label: 'Access the CRD IDE' },
      ]}
      install={`helm install weebo-notify \\
  oci://ghcr.io/batleforc/charts/weebo-notify \\
  --namespace weebo-notify --create-namespace`}
      example={{
        title: 'A channel',
        yaml: `apiVersion: notify.weebo.io/v1alpha1
kind: NotifyChannel
metadata:
  name: alerts
  namespace: team-a
spec:
  name: Alerts
  access:
    - oidcGroup: ops
      scopes: [subscribe, manage]
    - serviceAccount: grafana
      scopes: [publish]`,
      }}
      features={[
        {
          title: 'Checked by Notify itself',
          body: "The operator renders config.toml and parses it with Notify's own notify-config before writing it: an object Notify would refuse is left out alone, with the reason in its status.",
          href: '/docs',
          link: 'Overview',
        },
        {
          title: 'Confirmed loaded',
          body: 'Notify reloads its file when it changes; an object is Ready once every pod of its instance reports the digest of a configuration that holds it.',
          href: '/docs/crds/notifychannel',
          link: 'NotifyChannel',
        },
        {
          title: 'On the weebo kit',
          body: 'Same status, Events, metrics, chart and release pipeline as every weebo operator.',
          href: '/docs/reference/reason-codes',
          link: 'Reason codes',
        },
      ]}
    />
  );
}
