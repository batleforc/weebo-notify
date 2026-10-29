# weebo-notify (opérateur) — plan de conception

Document de référence de l'opérateur Kubernetes de weebo-notify
(`~/git/weebo-notify-v2`). À lire avant tout changement non trivial.
Décisions prises le 2026-10-29 (Q1–Q30 de la session de cadrage).

## Objectif

Déployer weebo-notify et gérer ses objets déclarés par configuration
(canaux managés, service accounts, trust policies) depuis des CR, en
GitOps, avec le même niveau d'exigence que les autres opérateurs weebo.

## Constat de départ (weebo-notify-v2)

- Les objets managés et le rôle d'admin plateforme n'existent **que**
  dans le TOML du serveur (`[[channels.managed]]`, `[[service_accounts]]`,
  `[[transforms]]`, `[[trust_policy]]`, `[admin] oidc_groups`) ; le serveur
  les réconcilie
  lui-même (boot, `SIGHUP`, timer de 60 s sur la config en mémoire).
- L'API REST refuse les identités machine pour tout ce qui est admin :
  création de canal et de clé = session humaine, service account = admin
  plateforme (claim `groups` OIDC uniquement). Un opérateur muni d'une
  clé d'API ne peut rien créer.
- Un seul fichier de config (pas d'include). `SIGHUP` relit `channels`,
  `service_accounts`, `transforms`, `trust_policy`, `sinks` (sauf `fingerprint_key`,
  `previous_fingerprint_keys`, `dead_letter_retention`) et `reconcile` ;
  le reste exige un redémarrage, dont `admin.oidc_groups` (sans
  avertissement aujourd'hui).
- Retrait d'un objet de la config : canal archivé (redevient dynamique)
  ou supprimé avec ses notifications selon `[channels] on_removed`
  (global) ; service account désactivé, clés révoquées ; garde
  `max_removals` (5) qui refuse les retraits sans bloquer le reste, signalé
  seulement en log et en métrique.
- Postgres obligatoire (bus d'événements + verrou de leader), S3 ou
  filesystem (PVC, une seule réplique), sous-commande `migrate`, web nginx
  séparé, `/healthz` `/readyz` `/metrics` sur le port principal.

Conséquence : le modèle `RemoteGateway` (API distante) du template ne
convient pas. Le modèle est celui des opérateurs qui déploient leur
produit (batlehub, angos) : couche « managed workload » du kit.

## Décisions

### Identité

- Repo séparé `~/git/operator/weebo-notify`, Apache-2.0 (`LICENSE.md`),
  groupe `notify.weebo.io/v1alpha1`.
- Généré par `task template:render` du kit v0.24.0 puis déplacé. La
  gateway factice du template est remplacée à la main par `OwnedObjects` ;
  chaque manque du template est noté dans le `docs/plan.md` du kit. Une
  variante « managed » du template viendra plus tard, dans une release du
  kit (trois utilisateurs : batlehub, angos, notify).

### Kinds (v0.1, tous namespacés)

**`NotifyLocalInstance`** (`nli`), mode managed seulement (pas de mode
external : sans API admin il n'aurait rien à piloter ; pas de
`NotifyInstance` cluster).

- Deployment serveur, PDB, NetworkPolicy ; Deployment web optionnel
  (`spec.web`, actif par défaut) dont `/env.js` vient de l'OIDC de
  l'instance.
- Job `notify-server migrate` par version d'image avant le rollout,
  condition `Migrated` (avec la version) ; pods en
  `MIGRATE_ON_START=false`.
- Postgres apporté : `spec.database.urlSecretRef`. Pas de CNPG.
- Pièces jointes S3 par défaut (endpoint, bucket, Secret d'identifiants) ;
  filesystem = PVC créée par l'opérateur + `token_secret`, refusé à
  l'admission si `replicas > 1`.
- Ingress API et web (host, classe, Secret TLS, annotations). HTTPRoute
  plus tard. L'Ingress API ne route que `/api/v1` (ni `/healthz`, ni
  `/readyz`, ni `/metrics`).
- URL publique de l'API (`WNK_API_BASE_URL` du web) :
  `spec.server.publicUrl` si fourni (Gateway API, proxy externe), sinon
  l'hôte de `ingress.api`, sinon l'URL du Service avec l'avis
  `ApiNotExposed`. Le lien des mails (`smtp.permalinkBase`, `<base>/n/<id>`)
  est une route du web : par défaut l'hôte de `ingress.web`, sinon pas de
  lien.
- Suppressions : un objet est libéré quand un fichier appliqué par tous les
  pods ne le contient plus ; un canal ou compte dont Notify a refusé le
  retrait garde son finalizer (`RemovalRefused`). Une instance reste tant
  que des objets vivants y sont liés (`InstanceInUse`) ; force-orphan
  libère toujours.
- ServiceMonitor par instance (actif si le CRD existe).
- Config cœur typée (OIDC, groupes admin, SMTP, VAPID, OTLP, rétention…)
  + `extraConfig` TOML, qui ne peut pas contenir `channels.managed`,
  `service_accounts`, `transforms` ni `trust_policy` (refus à l'admission).
- `spec.channels.onRemoved: Archive | Delete` (défaut `Archive`).
- `spec.reconcile.maxRemovals` / `allowMassRemoval` (défaut de notify).
- `AttachNamespaces` (défaut `Same`) pour les `instanceRef` d'autres
  namespaces.
- Garde de version façon angos : jamais de downgrade, au plus une mineure
  d'avance sauf annotation `approved-upgrade` qui nomme le tag.

**`NotifyChannel`** (membres, bindings, rétention, sinks inline),
**`NotifyServiceAccount`**, **`NotifyTransform`**, **`NotifyTrustPolicy`** :
`instanceRef` vers
une instance. `spec.id` vaut `metadata.name` par défaut ; en cas de
collision d'id sur une instance, la CR la plus ancienne gagne, les autres
passent `Conflict`. La description CRD de `NotifyServiceAccount` dit que
sa suppression désactive le compte et révoque ses clés.

**`NotifyTransform`** rend une entrée `[[transforms]]` (RFC 0003 de
notify) : transformation CEL d'un webhook JSON entrant en notifications.
Spec = les clés de notify telles quelles (`channels`, `lang`, `split`,
`maxItems`, `title`, `titleFallback`, `body`, `link`, `externalId`,
`groupKey`, `tags`, `priority`, `status`, `state`, `ack`, `update`,
`raw`), les expressions CEL en chaînes. Admission : compilation par
`notify-config` (même code que le serveur) ; une expression qui ne
compile pas est refusée au `kubectl apply`. `channels` nomme des ids de
canaux de la même instance ; un id qui n'est pas une `NotifyChannel`
connue n'est pas refusé (il peut être dynamique) mais donne un avis sur
la CR. Rechargeable à chaud, sans état en base : le retrait est immédiat
et n'entre pas dans la garde `max_removals`.

**`NotifyApiKey`** : v0.2 (voir notify-v2, ADR-0033).

### Rendu et validation de la config

- Les types de config de notify sortent de `notify-server` dans une crate
  serde seule `notify-config` (ADR-0031), dépendance git épinglée sur un
  tag de notify. Admission et reconcile passent par le parsing et la
  validation **structurelle** de notify : une CR invalide est refusée au
  `kubectl apply`.
- Les secrets ne vont jamais dans la ConfigMap : le fichier est rendu sans
  eux, les pods les reçoivent en variables `WNK_*` depuis les Secrets
  labellisés. Les contrôles qui ont besoin de leur valeur (références
  `env:`/`file:`, longueur des fingerprint keys et du `token_secret` du
  stockage filesystem) restent au boot du serveur : un secret invalide
  remonte en rollout échoué sur l'instance, pas en refus d'admission.
- La séparation boot / rechargeable vient de `reloaded_from` de
  `notify-config` (le hash de boot est celui de
  `config.reloaded_from(Config::default())` plus les émetteurs de
  fédération), pas d'une copie dans l'opérateur. `unreloadable` ne sert pas :
  il ne liste qu'une partie des clés lues au boot (il manque rétention, CORS,
  télémétrie, push).
- Clés générées par l'opérateur quand le spec n'en fournit pas (une seule
  par instance, partagée par tous les pods) : fingerprint key des sinks,
  clé de signature des liens du stockage filesystem, clé Ed25519 de
  fédération (`spec.federation.signingKeySecretRef`, sans laquelle Notify
  ignore trust policies et `publishers` des canaux). La rotation de la
  fingerprint key passe par `spec.sinks.previousFingerprintKeySecretRefs`.
- Secrets des sinks d'un canal d'un autre namespace : l'opérateur les copie
  dans le Secret `<instance>-sinks`, monté en répertoire et référencé en
  `file:` (ajouter un sink ne redémarre rien). Deux consentements :
  l'instance accepte le namespace (`objectSelector.namespaces`) et le Secret
  porte le label `credential`.
- Chaque release de l'opérateur fixe l'image notify par défaut
  (`ghcr.io/batleforc/weebo-notify-v2`, `-v2-web`, taguées `vX.Y.Z`) au tag de sa dépendance
  `notify-config` et déclare une plage supportée ; hors plage :
  `UnsupportedVersion` (sauf `approved-upgrade`). Allow-list d'images par
  défaut : ces deux dépôts.

### Application des changements

- Clés lues au boot (dont `admin.oidc_groups`) → hash de boot → rollout.
- Sections rechargeables (canaux, comptes, transforms, trust policies,
  sinks) →
  ConfigMap réécrite en place ; notify surveille son fichier et recharge
  seul (ADR-0031, donc dès la v0.1). Tant que le digest appliqué n'est pas
  dans `/healthz` (ADR-0032), l'opérateur ne peut pas confirmer : l'objet
  reste `Reconciling` jusqu'à un délai (sync kubelet + marge), puis
  `Ready`. Avec ADR-0032, il confirme pod par pod.
- Pas de `pods/exec` : l'opérateur n'envoie jamais `SIGHUP`.

### Status des objets

- `Ready` = l'objet est dans une config dont chaque pod prêt rapporte le
  digest. Validation locale (ids, bindings, existence des Secrets
  `secretRef` des sinks) à l'admission ou au reconcile.
- Retrait refusé par la garde : la CR garde son finalizer et passe
  `RemovalRefused` (ids refusés exposés dans `/healthz`, ADR-0032). Sinon
  la suppression d'un namespace laisserait des canaux managés sans CR.

### Sécurité

- Tout Secret lu par les pods porte `notify.weebo.io/credential: "true"`
  (`pod_safety` du kit) : URL de la base, S3, SMTP, VAPID, `secretRef` des
  sinks, fingerprint keys, clé de fédération.
- Pas de RBAC `pods/exec`.

### Chart et observabilité

- Chart de l'opérateur = enveloppe de `weebo-operator-lib` ; il porte une
  seule fois la PrometheusRule et le dashboard Grafana de notify.
- `charts/weebo-notify` reste dans notify-v2 pour qui n'utilise pas
  l'opérateur ; aucun template partagé. Sa dépréciation sera revue quand
  l'opérateur fera tourner les clusters.

### Tests

- Standard du kit : unitaires, contrat, envtest.
- Golden tests du `config.toml` et des objets rendus.
- Aller-retour par `notify-config` (ce que rend l'opérateur, notify le
  parse).
- e2e k3d (port 8443) avec Postgres et RustFS : vrai notify, une
  `NotifyChannel` appliquée, une notification publiée. Tâche et job CI à
  part, hors `test`.

### Docs

- Site sur `@weebo/operator-docs`, pages CRD générées, page de migration
  depuis le chart Helm (`values.yaml` → `NotifyLocalInstance`).
- Entrée au catalogue CRD du kit après la release v0.1.0.

## Travail amont dans weebo-notify-v2

| ADR | Changement | Bloque |
|---|---|---|
| 0031 | crate `notify-config` (types, validation structurelle, classification boot / rechargeable) ; le serveur recharge son fichier quand il change ; avertissement quand `admin.oidc_groups` change sans redémarrage | v0.1 |
| 0032 | digest de la config appliquée et ids des retraits refusés dans `/healthz` | non (attente à délai en attendant) |
| 0033 | principal « opérateur » déclaré en config, autorisé à créer des clés de service accounts | v0.2 (`NotifyApiKey`) |

## Feuille de route

1. notify-v2 : ADR-0031 livré et tagué.
2. Rendu du template, licence, `task init` / `doctor` / `recu` /
   `test:all`.
3. `NotifyLocalInstance` : workload, migration, rendu de config, garde de
   version, admission.
4. `NotifyChannel`, `NotifyServiceAccount`, `NotifyTransform`,
   `NotifyTrustPolicy` : rendu, collisions, finalizers, `onRemoved`.
5. Golden, aller-retour, envtest, e2e k3d.
6. Chart, docs, page de migration → **v0.1.0**, puis catalogue du kit.
7. notify-v2 ADR-0032 → confirmation par digest et `RemovalRefused`
   (mineure).
8. notify-v2 ADR-0033 → `NotifyApiKey` (**v0.2**).

## Définition de fini (v0.1.0)

Instance + quatre kinds d'objets, tests ci-dessus verts, chart publié,
release taguée ; puis entrée au catalogue CRD du kit.
