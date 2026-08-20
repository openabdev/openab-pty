# Deploying openab-pty on Kubernetes

Operator notes. The manifest is [`deploy/k8s/pod-tailscale.yaml`](../deploy/k8s/pod-tailscale.yaml);
this explains what it needs around it and which failures you should expect on the
way. Verified on k3s (including OrbStack).

For ECS Fargate instead, see [`ecsctl-howto.md`](ecsctl-howto.md).

## Shape of the thing

One pod, two containers, sharing a network namespace:

- **`openab-pty`** runs the runtime. It binds `127.0.0.1:8090` and nothing else.
  There is no `ports:` block, no `hostPort`, and no Service — by design.
- **`tailscale`** is a userspace `tailscaled` that gives the pod its own tailnet
  identity and forwards inbound connections to loopback.

The consequence worth internalising: **there is no Kubernetes-native way in.** No
Service, no Ingress, no port-forward in normal use. If the tailnet is down, the
terminal is unreachable, and that is the intended failure mode rather than a
misconfiguration.

Userspace networking is what keeps this honest — it needs neither `NET_ADMIN` nor
`/dev/net/tun`, so both containers keep `runAsNonRoot`, all capabilities dropped,
and a read-only root filesystem.

## Prerequisites

- A cluster you can create pods and secrets in. No CRDs, no operator, no Helm.
- A **reusable or ephemeral** tailnet auth key. Prefer ephemeral: a replaced pod
  then leaves the tailnet instead of accumulating dead nodes. Accumulating them is
  a real failure that has already happened elsewhere in this fleet — a new
  `<host>-N` device on every restart.
- An admin credential pair.

## 1. Generate the admin credential

The runtime only ever holds a non-reversible `sha256:` verifier. The credential
itself must never be in the manifest, in argv, or on disk in the cluster.

```bash
docker run --rm --entrypoint /usr/local/bin/openab-pty \
  ghcr.io/openabdev/openab-pty:pre-beta-kiro --generate-admin-credential
```

That prints the credential and its hash. **The credential goes to your client's
keychain and nowhere else.** Only the hash goes into the cluster. There is no
recovery path: lose it and you redeploy with a new pair.

## 2. Create the namespace and secrets

```bash
kubectl create namespace openab-pty

kubectl -n openab-pty create secret generic openab-pty \
  --from-literal=admin-hash='sha256:<64 hex from step 1>'

kubectl -n openab-pty create secret generic openab-pty-tailscale \
  --from-literal=authkey='tskey-auth-…'
```

Two secrets rather than one because they have different lifetimes: the auth key is
consumed at first boot and can be rotated without touching the admin hash.

## 3. Choose a variant and deploy

The image tag selects which agent CLI is baked in:

```bash
# edit the image line, or patch it inline
kubectl -n openab-pty apply -f deploy/k8s/pod-tailscale.yaml
```

Channels are `pre-beta-<variant>` (built from `openab:pre-beta-<variant>`) and
`beta-<variant>`. For anything you want to be able to identify later, deploy the
immutable `<variant>-<sha>` tag instead — a moving tag makes "which code was
running" unanswerable after the fact.

`native` carries no agent CLI. Every other variant bundles a vendor's proprietary
CLI under that vendor's terms — see [`../NOTICE`](../NOTICE).

## 4. Confirm it came up, and that it refuses what it should

```bash
kubectl -n openab-pty get pod openab-pty -w
kubectl -n openab-pty logs openab-pty -c openab-pty
kubectl -n openab-pty logs openab-pty -c tailscale | grep -i "logged in\|Success"
```

Then verify the sandbox properties actually hold, rather than assuming the image
kept them:

```bash
kubectl -n openab-pty exec openab-pty -c openab-pty -- id
# uid=1000 — not root

kubectl -n openab-pty exec openab-pty -c openab-pty -- sh -c 'command -v sudo || echo "no sudo"'

kubectl -n openab-pty exec openab-pty -c openab-pty -- sh -c 'touch /probe 2>&1 || echo "rootfs read-only"'

kubectl -n openab-pty exec openab-pty -c openab-pty -- \
  sh -c 'ls /var/run/secrets/kubernetes.io 2>&1 || echo "no service-account token"'
```

The last one is `automountServiceAccountToken: false` doing its job, and it is
pod-level — which is why the sidecar cannot use `TS_KUBE_SECRET` for state and
uses `TS_STATE_DIR` instead. Enabling it for the sidecar would hand a token to the
terminal container too.

## 5. Attach

Point a client at the pod's tailnet address on port 8090 with the admin
credential. The wire protocol is [`../runtime/CLIENT-CONTRACT.md`](../runtime/CLIENT-CONTRACT.md);
§8 is a minimum viable client.

```bash
tailscale status | grep openab-pty     # find the address
```

## Failures worth knowing about in advance

**`CreateContainerConfigError` / pod never starts.** Almost always a missing
secret key. The names must match exactly: secret `openab-pty` key `admin-hash`,
secret `openab-pty-tailscale` key `authkey`. `kubectl -n openab-pty describe pod
openab-pty` names the missing one.

**Exit code 64 immediately.** The entrypoint's own check: `PTY_ADMIN_HASH` is
absent or empty. It must be `sha256:` followed by exactly 64 lowercase hex
characters — the validator rejects uppercase and any other prefix.

**`Permission denied` writing config, or a crash loop on start.** The entrypoint
materialises `config.toml` under `/tmp`, and the root filesystem is read-only. The
`tmp` `emptyDir` is load-bearing; removing it to "tidy up" breaks startup.

**The pod runs but is unreachable on the tailnet.** Check the sidecar log first.
An expired or already-consumed auth key is the usual cause, and it does not stop
the pod — the terminal container is perfectly healthy and simply has no way in.
A reusable key that was consumed by a previous pod fails this way.

**`403` creating the namespace.** Your context cannot create namespaces. Use an
existing one; nothing in the manifest depends on the name.

**HTTP `429` on your first admin request.** This is the one that costs people an
afternoon. The runtime arms a failure backoff on *every rejected* admin attempt,
so an unauthenticated poller — a liveness probe, a "is it up yet" loop, a browser
tab — will throttle the source before your real request arrives. Two rules:

- Send the credential on *every* request, including probes. An authenticated probe
  consumes no failure budget.
- When testing whether the admin plane refuses correctly, treat both `401` **and**
  `429` as a refusal. Only a `200` without a credential is a failure. §6 of the
  client contract explains why disambiguating `401` is required rather than
  optional.

**A process survives its session.** Not a bug — Tier 1 is the only kill domain
implemented, and a process that leaves its process group may outlive its session
until the pod is replaced. Teardown is best-effort and is labelled as such
everywhere it is surfaced. Please do not report it as a vulnerability; see
[`../SECURITY.md`](../SECURITY.md).

## Cleaning up

```bash
kubectl -n openab-pty delete pod openab-pty
kubectl -n openab-pty delete secret openab-pty openab-pty-tailscale
```

`restartPolicy: Never` and an `emptyDir` workspace mean deleting the pod discards
the session state with it. There is nothing to drain and no volume to reclaim.
With an ephemeral auth key the tailnet node disappears on its own; with a reusable
key, remove it from the Tailscale admin console.
