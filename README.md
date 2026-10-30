# weebo-notify

Kubernetes operator running [weebo-notify](https://github.com/batleforc/weebo-notify-v2)
and the objects it is configured with, built on
[weebo-operator-kit](https://github.com/batleforc/weebo-operator-kit).
`docs/plan.md` is the design.

| Kind                   | Short | What it does                                                         |
| ---------------------- | ----- | -------------------------------------------------------------------- |
| `NotifyLocalInstance`  | `nli` | A Notify the operator runs in its namespace, and its `config.toml`   |
| `NotifyChannel`        | `nch` | A managed channel (`[[channels.managed]]`) of an instance            |
| `NotifyServiceAccount` | `nsa` | A managed service account (`[[service_accounts]]`)                    |
| `NotifyTransform`      | `ntr` | A JSON webhook transform (`[[transforms]]`, CEL)                      |
| `NotifyTrustPolicy`    | `ntp` | A federation trust policy (`[[trust_policy]]`)                        |
| `NotifyApiKey`         | `nak` | A service account's API key, minted into Secrets or Vault             |

All namespaced. The objects are rendered into their instance's
configuration (parsed and checked by Notify's own `notify-config`); Notify
reloads the file when it changes. An API key is minted through Notify's
API instead, as the instance's key issuer, and written to its
`secretTargets` (Kubernetes Secrets, or the operator's Vault).

## Layout

Hexagonal, one crate per layer, same rules as every weebo operator:

| Crate               | Holds                                                                    |
| ------------------- | ------------------------------------------------------------------------ |
| `domain`            | the reason codes (`reason_codes!`), no dependency                        |
| `api`               | the CRD types (`#[derive(CustomResource)]`), the kit's shared field types |
| `application`       | ports, the `config.toml` render (`render`), the `reconcile_*` use cases   |
| `adapters-outbound` | the ports on the Kubernetes API (instances, bound objects, Secrets, status), Notify's key routes, Vault |
| `adapters-inbound`  | the controllers and the webhook router (health and scope routes for now)  |
| `operator`          | the binary: telemetry, leader election, webhook server, controllers      |
| `crdgen`            | writes `deploy/crd/` from `api` (CRDs, and `links.json` for the CRD IDE)  |
| `testkit`           | envtest and fakes for the integration tests                               |

`deploy/charts/weebo-notify` is a thin chart on the kit's library
chart: `templates/_params.tpl` holds what is the operator's own (group,
kinds, env, egress), `values.yaml` follows the kit's values contract.

## First steps

The project is licensed `Apache-2.0` (`Cargo.toml`, the image's labels):
add the matching `LICENSE.md`.

```bash
task init     # toolchain (mise) and mise.lock — commit it, cog.toml from the kit, git hooks
task doctor   # envtest prerequisites
task recu     # CRDs (crdgen) and their copies in the chart
task test:all # unit, contract and envtest tests — creates Cargo.lock: commit it
```

## Tasks

```bash
task lint        # fmt --check, clippy -D warnings, actionlint, hadolint
task lint:helm   # helm lint + the kit's smoke renders
task test        # unit + contract tests
task test:all    # + envtest integration tests
task coverage    # cargo llvm-cov, fails under 85%
task audit       # cargo-deny + trivy + trivy config
task kit:sync TAG=vX.Y.Z  # move to another kit release (crates, tasks, CI, chart, cog.toml)
task golden:update  # rewrite the golden files (rendered config.toml, an instance's objects)
task e2e         # kind e2e on a real Notify (Postgres, RustFS, Dex); CI runs it on PRs and before a release
task docs        # the docs site (docs/, @weebo/operator-docs) in dev mode; task recu regenerates its CRD pages
```

`lint`, `audit`, `coverage`, `doctor` and the chart tasks are the kit's
(`tasks/*.yaml`, included remotely and pinned by checksum); CI calls the
kit's reusable `checks.yml`.
