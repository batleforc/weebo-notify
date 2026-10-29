import { defineSite } from '@weebo/operator-docs/site';

// What makes this site weebo-notify's; everything else comes from
// @weebo/operator-docs.
export const site = defineSite({
  name: 'weebo-notify',
  description:
    'Run weebo-notify — its instances, channels, service accounts, transforms and trust policies — as Kubernetes resources.',
  git: {
    user: 'batleforc',
    repo: 'weebo-notify',
    branch: 'main',
    contentDir: 'docs/content/docs',
  },
});
