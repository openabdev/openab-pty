# Deploying openab-pty

Both shapes below bind the runtime to **loopback only** and reach it over a
WireGuard tailnet through a userspace `tailscaled` sidecar sharing the network
namespace. Nothing is published on a host interface, and the runtime's own
fail-closed guard permits a loopback bind unconditionally — so
`tls_terminated_upstream` is `false` and honest rather than claiming an upstream
TLS terminator that does not exist.

## Kubernetes — `k8s/pod-tailscale.yaml`

Verified on k3s v1.36.2. Sandbox properties confirmed from inside a live session:
`uid=1000`, no `sudo`, no service-account token, read-only root filesystem, and
the tailscale state directory unreadable because it lives in the sidecar.

`TS_KUBE_SECRET` is deliberately empty. containerboot otherwise stores state in a
Kubernetes Secret, which needs a service-account token — and
`automountServiceAccountToken: false` is one of the properties this deployment
exists to demonstrate. That setting is pod-level, so enabling it for the sidecar
would hand a token to the terminal container too.

## ECS Fargate — `ecs/taskdef.json`

Verified on Fargate. Three platform-specific things this encodes, none of which
are documented anywhere obvious:

1. **`awslogs-create-group` needs `logs:CreateLogGroup`**, which the managed
   `AmazonECSTaskExecutionRolePolicy` does not grant. Create the log group first.
2. **Fargate bind mounts are root-owned and there is no `fsGroup` equivalent**, so
   a `uid=1000` container cannot write to them. The `init-perms` container runs as
   root, chowns the volumes and exits, ordered by `dependsOn: SUCCESS`.
3. **The tailscale image's containerboot fails here**, reporting a socket timeout
   in under a second. The task definition starts `tailscaled` and runs
   `tailscale up` directly instead, which matches OAB's documented userspace
   recipe and worked first time.

Replace `SUFFIX` in the secret ARN with the real one, and point the image at your
own registry.

## CI-built images (GHCR, public)

`.github/workflows/image.yml` builds this Dockerfile on every push to `main` and
every `v*` tag, as a matrix over every agent-CLI variant the openab base ships
except `agentcore`. Images go to `ghcr.io/openabdev/openab-pty`, authenticated
with the workflow's own `GITHUB_TOKEN` — no long-lived credential anywhere.

Two channels, each tracking the *same* channel of the base so a tag never claims
more stability than what it was built from:

| Trigger | openab-pty tag | Built FROM |
|---|---|---|
| push to `main` | `pre-beta-<variant>` | `openab:pre-beta-<variant>` |
| `v*` tag | `beta-<variant>` | `openab:beta-<variant>` |

Every build also gets an immutable `<variant>-<sha>`. Deploying only the moving
tag makes "which code is running" unanswerable after the fact.

There is deliberately no `stable` channel: openab publishes no `stable-*` tag, so
an `openab-pty:stable-*` would have nothing to track and would in fact be built
from a pre-beta or beta base — a label that lies. Add one when the base grows one.

**Public images need no pull secret.** That is the practical reason for GHCR over
the private ECR this used to push to: the ECR token expired every 12 hours and had
to be refreshed in every cluster before a deploy would pull. See [`../NOTICE`](../NOTICE)
for what the images contain and under whose terms.

The base image is pinned by digest, resolved per variant at build time and recorded
in the job log, so a base change cannot silently alter the output of an unchanged
commit. A scheduled workflow (`bump-base-image.yml`) re-resolves the tracked tag
weekly and opens a PR if it moved,
so staying current is a reviewed decision rather than a build silently
re-resolving a tag on every run.
