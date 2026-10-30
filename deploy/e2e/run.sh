#!/usr/bin/env bash
# kind e2e (`task e2e`, .github/workflows/e2e.yml): deploys cert-manager,
# what Notify needs (deps.yaml: Postgres, RustFS, Dex) and the operator
# chart into the current kube context, then walks the golden path — a
# NotifyLocalInstance runs the real Notify (migration Job, server, web), a
# NotifyChannel reaches its pods, Alice publishes to it with her Dex token
# and reads it back, a NotifyApiKey's key (minted by the operator, written
# to a Secret) publishes too and is revoked with its CR, the channel's
# deletion archives it (its history stays) — and checks the instance's
# deletion.
#
# Env: IMAGE (operator image already loaded in the cluster, default
# weebo-notify-operator:e2e), CERT_MANAGER_VERSION (default: the pinned
# release below; its manifest is checksum-verified only for the pinned one).
# -E: functions inherit the ERR trap that dumps diagnostics.
set -Eeuo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CHART="$HERE/../charts/weebo-notify"
IMAGE="${IMAGE:-weebo-notify-operator:e2e}"
# Pinned: bump both together (sha256 of the release's cert-manager.yaml).
CERT_MANAGER_PINNED=v1.21.2
CERT_MANAGER_SHA256=e03b668ec8675214af6b0a671699d088f2601fa3878e0dbe1b41d3feafd1879f
CERT_MANAGER_VERSION="${CERT_MANAGER_VERSION:-$CERT_MANAGER_PINNED}"
OPERATOR_NS=weebo-notify-system
RELEASE=weebo-notify
PASSWORD=e2e-only-insecure-password
# Dex's client (deps.yaml).
CLIENT=weebo-notify
CLIENT_SECRET=e2e-only-insecure-secret
WAIT=300s

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
fail() {
  printf '\033[31mFAIL: %s\033[0m\n' "$*" >&2
  dump
  exit 1
}

# Diagnostics for whatever failed: operator and Notify logs, CRs, events.
dump() {
  set +e
  step "diagnostics"
  kubectl -n "$OPERATOR_NS" get pods -o wide
  kubectl -n "$OPERATOR_NS" logs -l app.kubernetes.io/name=weebo-notify --tail=200 --prefix --all-containers
  kubectl -n team get notifylocalinstances,notifychannels,notifyserviceaccounts,notifyapikeys -o yaml
  kubectl -n team get all,jobs
  kubectl -n team logs -l app.kubernetes.io/name=notify --tail=100 --prefix --all-containers
  kubectl -n notify-deps get pods -o wide
  kubectl get events -A --sort-by=.lastTimestamp | tail -n 50
}
# dump turns errexit off: leave right after it.
trap 'dump; exit 1' ERR

ready() { kubectl wait --for=condition=Ready=True --timeout="$WAIT" "$@"; }

FORWARDS=()
trap 'kill "${FORWARDS[@]}" 2>/dev/null || true' EXIT
# port-forward <namespace> <service> <local:remote>, waiting until it
# answers.
forward() {
  kubectl -n "$1" port-forward "svc/$2" "$3" >/dev/null 2>&1 &
  FORWARDS+=("$!")
  for _ in $(seq 1 30); do
    (exec 3<>"/dev/tcp/127.0.0.1/${3%%:*}") 2>/dev/null && return
    sleep 1
  done
  fail "port-forward to $1/$2 never answered"
}

step "cert-manager ($CERT_MANAGER_VERSION)"
manifest="$(mktemp)"
curl -fsSLo "$manifest" \
  "https://github.com/cert-manager/cert-manager/releases/download/$CERT_MANAGER_VERSION/cert-manager.yaml"
if [ "$CERT_MANAGER_VERSION" = "$CERT_MANAGER_PINNED" ]; then
  echo "$CERT_MANAGER_SHA256  $manifest" | sha256sum -c - >/dev/null || fail "cert-manager.yaml checksum mismatch"
fi
kubectl apply -f "$manifest"
kubectl -n cert-manager wait --for=condition=Available --timeout="$WAIT" deploy --all

step "Postgres, RustFS, Dex"
kubectl apply -f "$HERE/deps.yaml"
kubectl -n notify-deps wait --for=condition=Available --timeout="$WAIT" deploy --all
forward notify-deps rustfs 19000:9000
# The attachments bucket, signed by curl itself (no S3 client needed).
# RustFS answers 503 until its storage is up.
for _ in $(seq 1 30); do
  curl -fsS -X PUT --aws-sigv4 "aws:amz:us-east-1:s3" --user "notify-e2e:$PASSWORD" \
    http://127.0.0.1:19000/notify && break
  sleep 2
done
curl -fsS -o /dev/null --aws-sigv4 "aws:amz:us-east-1:s3" --user "notify-e2e:$PASSWORD" \
  http://127.0.0.1:19000/notify || fail "no attachments bucket"
forward notify-deps dex 15556:5556

step "operator chart ($IMAGE)"
# The kit's library chart, as Chart.yaml pins it.
helm dependency build "$CHART"
helm upgrade --install "$RELEASE" "$CHART" \
  --namespace "$OPERATOR_NS" --create-namespace \
  --set image.repository="${IMAGE%:*}" --set image.tag="${IMAGE##*:}" \
  --set image.pullPolicy=Never \
  --set extraEnv[0].name=RUST_LOG --set extraEnv[0].value=info \
  --wait --timeout 5m
kubectl -n "$OPERATOR_NS" rollout status "deploy/$RELEASE" --timeout="$WAIT"

step "golden path: the instance"
kubectl create namespace team --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n team create secret generic db \
  --from-literal=url="postgres://notify:$PASSWORD@postgres.notify-deps.svc.cluster.local:5432/notify"
kubectl -n team create secret generic s3 \
  --from-literal=AWS_ACCESS_KEY_ID=notify-e2e --from-literal=AWS_SECRET_ACCESS_KEY="$PASSWORD"
vapid="$(mktemp)"
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$vapid"
kubectl -n team create secret generic vapid --from-file=key.pem="$vapid"
# The instance's pods read only the Secrets labelled for it.
kubectl -n team label secret db s3 vapid notify.weebo.io/credential=true
# Through the admission webhook: retry while cert-manager injects its CA
# and its endpoints get ready.
applied=""
for _ in $(seq 1 30); do
  if kubectl apply -f "$HERE/instance.yaml"; then applied=1; break; fi
  sleep 2
done
[ -n "$applied" ] || fail "the instance was never admitted"
ready -n team notifylocalinstance/main
kubectl -n team wait --for=condition=Migrated=True --timeout=10s notifylocalinstance/main
kubectl -n team rollout status deploy/main --timeout="$WAIT"
forward team main 18080:8080
curl -fsS http://127.0.0.1:18080/readyz >/dev/null || fail "Notify is not ready"

step "golden path: Alice's token, her channel"
token() {
  curl -fsS http://127.0.0.1:15556/dex/token \
    -u "$CLIENT:$CLIENT_SECRET" \
    -d grant_type=password -d scope="openid email profile" \
    -d username=alice@e2e.local -d password="$PASSWORD" | jq -er .access_token
}
TOKEN="$(token)"
# The JWT's payload, base64url without padding.
payload="$(cut -d. -f2 <<<"$TOKEN" | tr '_-' '/+')"
while [ $(( ${#payload} % 4 )) -ne 0 ]; do payload="$payload="; done
SUB="$(base64 -d <<<"$payload" | jq -er .sub)"
notify() {
  curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    "http://127.0.0.1:18080/api/v1$1" "${@:2}"
}

# Her first request makes her a user: an `oidcSub` binding resolves only
# to someone who has logged in.
notify /me >/dev/null
kubectl apply -f "$HERE/api-key.yaml"
sed "s|__SUB__|$SUB|" "$HERE/channel.yaml" | kubectl apply -f -
ready -n team notifychannel/alerts

step "golden path: Alice publishes and reads it back"
# The binding resolves on the reload the channel caused, or on Notify's
# next reconcile pass (every 60s).
published=""
for _ in $(seq 1 24); do
  if notify /notifications -X POST \
    -d '{"channel": "alerts", "title": "e2e", "state": "open", "status": "info"}' 2>/dev/null \
    | jq -e '.notification.channel == "alerts"' >/dev/null; then
    published=1
    break
  fi
  sleep 5
done
[ -n "$published" ] || fail "Alice could never publish to alerts"
notify '/notifications?channels=alerts' \
  | jq -e 'any(.items[]; .head.content.title == "e2e")' >/dev/null || fail "not in Alice's inbox"

step "an API key: minted into a Secret, it publishes, revoked with its CR"
ready -n team notifyserviceaccount/ci
ready -n team notifyapikey/ci
KEY="$(kubectl -n team get secret ci-key -o jsonpath='{.data.NOTIFY_API_KEY}' | base64 -d)"
[[ "$KEY" == wnk_* ]] || fail "ci-key holds no Notify key"
[ "$(kubectl -n team get secret ci-key -o jsonpath='{.data.NOTIFY_URL}' | base64 -d)" = \
  "http://main.team.svc:8080" ] || fail "ci-key's NOTIFY_URL is not the instance's"
publish_with_key() {
  curl -fsS -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
    http://127.0.0.1:18080/api/v1/notifications -X POST \
    -d '{"channel": "alerts", "title": "from ci", "state": "open", "status": "info"}'
}
publish_with_key | jq -e '.notification.channel == "alerts"' >/dev/null || fail "the key could not publish"
notify '/notifications?channels=alerts' \
  | jq -e 'any(.items[]; .head.content.title == "from ci")' >/dev/null || fail "the key's notification is not in Alice's inbox"
kubectl -n team delete notifyapikey ci --wait --timeout="$WAIT"
kubectl -n team get secret ci-key >/dev/null 2>&1 && fail "ci-key outlived its NotifyApiKey"
publish_with_key >/dev/null 2>&1 && fail "the key still publishes after its NotifyApiKey was deleted"

step "deleting the channel archives it (onRemoved: Archive, the default)"
kubectl -n team delete notifychannel alerts --wait --timeout="$WAIT"
# Notify keeps an archived channel and its history, unmanaged from now on.
notify '/notifications?channels=alerts' \
  | jq -e 'any(.items[]; .head.content.title == "e2e")' >/dev/null || fail "the archived channel lost its history"

step "deleting the instance"
kubectl -n team delete notifylocalinstance main --wait --timeout="$WAIT"
for _ in $(seq 1 30); do
  kubectl -n team get deploy main >/dev/null 2>&1 || break
  sleep 2
done
kubectl -n team get deploy main >/dev/null 2>&1 && fail "the instance's Deployment outlived it"

step "e2e passed"
