# openab-pty

Remote sandboxed terminal sessions: a small runtime that hands out a shell inside
a locked-down container, plus the clients that attach to it.

**Private and unpublished.** No image is published, no chart ships, and there is no
user-facing documentation, per the Gate A decision recorded on the tracking issue
in `openabdev/openab`. The design of record is
[`docs/adr/openab-pty-runtime.md`](https://github.com/openabdev/openab/blob/main/docs/adr/openab-pty-runtime.md)
there, and it stays there: it is a decision record for the OAB project, not for
this repository.

## Layout

| Path | What |
|---|---|
| `runtime/` | The Rust runtime. Static musl binary, no dependency on the OAB monorepo. |
| `runtime/CLIENT-CONTRACT.md` | Implementation spec for any client, captured from the running service rather than transcribed from source. |
| `clients/macos/` | Native macOS client. SwiftTerm + `URLSessionWebSocketTask`, credential in the Keychain. |
| `deploy/` | Kubernetes pod and ECS Fargate task definitions, both verified. |

## What it is for

A terminal that lives beside your ACP agents' workspace, in a pod rather than on
your host. The differentiator is the deployment and credential model, not terminal
features: the shell opens as `uid 1000` with no `sudo`, no service-account token,
no host credentials, a read-only root filesystem, and an ephemeral workspace, and
it is reached over a WireGuard tailnet with per-session tokens that carry no
signing key.

## Two invariants that must not be quietly relaxed

1. **The admin plane's boundary is the credential.** A managed session *can* reach
   the listener over loopback — the claim is that it cannot authenticate to, or
   successfully invoke, admin operations, which is why the adversary test asserts
   a `401` rather than a refused connection. There is no in-container admin socket,
   so no code path treats being inside the container as authorization.
2. **Teardown is best-effort.** Tier 1 is the only kill domain implemented, and a
   process that leaves its process group may outlive its session until the pod or
   task is replaced. Label it as such wherever it is surfaced; the clients here do.

## Building

Neither half is built on a laptop.

```bash
# runtime (Linux host)
cd runtime && cargo build --release --target x86_64-unknown-linux-musl

# macOS client (build host with Xcode)
cd clients/macos && swift build -c release
```

## Status

Phase 1, dogfooded on a homelab k3s cluster and on ECS Fargate. Not gated for
release: the demand check on the tracking issue is still open. The largest known
gap is **local echo prediction** in the clients — the runtime measures 1.0 ms of
echo latency from its own host against 78–82 ms from a laptop over WiFi, so
perceived quality is set almost entirely client-side.
