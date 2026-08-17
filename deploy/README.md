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
