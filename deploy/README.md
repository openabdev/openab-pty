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

## CI-built image (ECR)

`.github/workflows/image.yml` builds this Dockerfile on every push to `main` and
tag, and pushes to a private ECR repository (`openab-pty` in `123456789012`,
`us-east-1`) — not GHCR. Authentication is GitHub OIDC to a scoped IAM role
(`openab-pty-ci`, trust condition restricted to `refs/heads/main` and
`refs/tags/*` in this repository), so no long-lived credential is stored
anywhere. The role can push and pull exactly this one ECR repository and
nothing else.

This does not touch Gate B. Gate B on openabdev/openab#1479 governs *publishing*
an image, chart or docs; a private-account ECR push that nothing points a user
at is neither.

The base image (`ghcr.io/openabdev/openab`) is pinned by digest, matched to the
fleet's `pre-beta-kiro` tag rather than `:latest` — the two were different
images (openab 0.10.0 vs 0.9.0) when this was pinned. A scheduled workflow
(`bump-base-image.yml`) re-resolves that tag weekly and opens a PR if it moved,
so staying current is a reviewed decision rather than a build silently
re-resolving a tag on every run.
