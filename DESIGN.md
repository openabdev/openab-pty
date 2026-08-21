# openab-pty design

**This is not the design of record.** The design of record is
[`docs/adr/openab-pty-runtime.md`](https://github.com/openabdev/openab/blob/main/docs/adr/openab-pty-runtime.md)
in `openabdev/openab`, and it stays there — it answers *why each decision was
made*. This document answers two different questions: **why this project exists
at all**, and **where each decision landed in this repository** — which module
enforces it, which test verifies it. The intended reader is a contributor about
to change the code or a reviewer about to judge it.

Document division of labour, so nothing here is repeated elsewhere:

| Document | Reader | Question it answers |
|---|---|---|
| [`README.md`](README.md) | user, operator | what is this, how do I deploy it |
| [`runtime/CLIENT-CONTRACT.md`](runtime/CLIENT-CONTRACT.md) | client implementer | what does the wire look like |
| [`SECURITY.md`](SECURITY.md) | vulnerability reporter | what is claimed, what is a finding |
| this file | contributor, reviewer | why it exists, where the design lives in code |

---

## 1. Why this exists — five tenets

Every vendor is building its own desktop client, wired to its own models,
excluding everyone else's. We think that is backwards, and this project is the
disagreement written as a runtime:

1. **Any coding agent.** The user chooses the agent CLI; the runtime does not.
   Images ship one variant per agent CLI, plus a `native` variant that bundles
   none. A runtime that only worked with one vendor's agent would be part of the
   problem it exists to answer.

2. **Any environment that can run a sandbox.** Kubernetes anywhere — EKS, GKE,
   k3s, OrbStack — and serverless containers such as ECS Fargate. No
   vendor-managed control plane, no phone-home, no single blessed cloud. The
   runtime binds loopback only and delegates its network identity to a tailnet
   sidecar, which is precisely what makes it indifferent to what is underneath.

3. **Agents run remote; the experience stays local.** Your laptop is a
   viewport, not the host. The agents — and your shell beside them — live in
   pods. The measured basis for how the local feel is achieved is in
   [§7 of the client contract](runtime/CLIENT-CONTRACT.md): the runtime's own
   echo path costs 1.0 ms, the WiFi round trip costs 78–82 ms, so perceived
   latency is decided client-side, by design.

4. **Detach on the laptop, re-attach from anywhere.** A session outlives the
   connection that created it. Close the lid, pick up the phone, attach to the
   same session — scrollback intact, resumed from a byte offset, with any
   competing connection resolved by an explicit takeover rather than a corrupted
   stream. The honest boundary: a session survives *detach*, not *pod
   replacement* — see §5.

5. **The sandbox is a MUST.** No agent, and no shell next to one, runs on an
   unsandboxed host — least of all your own laptop. The shell opens as
   `uid 1000` with no `sudo`, no service-account token, no host credentials, a
   read-only root filesystem, and an ephemeral workspace. This is not a
   deployment option; the runtime refuses configurations that would weaken it
   (§3, listener guard).

In one sentence: **choose any agent, run it in any sandbox on any
infrastructure, fully distributed — and operate all of it from a single client.**

## 2. The complementary pair: ACP and PTY

[`openabdev/openab`](https://github.com/openabdev/openab) brokers ACP — you send
instructions from a chat client and care about the **outcome** the agent
delivers, not how it got there. `openab-pty` is the other mode: full
**observability** with a native terminal experience — you watch, inspect, and
act directly, in real time.

| | ACP (`openab`) | PTY (`openab-pty`) |
|---|---|---|
| Orientation | outcome — what did the agent deliver | process — what is happening right now |
| You are | the instructor | the operator at the keyboard |
| Session model | turn-based request/response | unbounded byte stream |

The two are complementary by construction, not by analogy: the PTY images are
`FROM ghcr.io/openabdev/openab`, so the shell opens **inside the same image the
agent runs in**, with the same CLI and the same workspace. When an agent leaves
something half-finished, you attach and look at it *in situ*. This shared-image
property is the design core, not a packaging convenience.

Neither requires the other. The session models are deliberately disjoint —
`session.rs` shares nothing with `openab-core`'s ACP pool, because a byte
stream's liveness is defined by bytes and sockets, not turns.

## 3. How the tenets become mechanisms

| Tenet | Mechanism in this repo | Verified by |
|---|---|---|
| Any agent | one image variant per CLI; `native` variant with none ([`NOTICE`](NOTICE)) | `image.yml` matrix |
| Any environment | loopback-only bind + tailnet sidecar as the sole network identity | `deploy/k8s/` and `deploy/ecs/`, both deployed and verified |
| Remote, feels local | server side already at 1.0 ms; contract §7 specifies client-side echo prediction + focus keepalive | latency measurements in contract §7 |
| Re-attach anywhere | `stream_offset` + `?since=` resume; single-attach CAS with close `4002`; per-session tokens with TTL | e2e tests, contract §4–5 |
| Sandbox is a MUST | uid 1000, no sudo, no SA token, read-only rootfs; **listener guard**: an off-loopback bind is refused unless an admin verifier is configured *and* the deployment declares `tls_terminated_upstream` | SECURITY.md claims 4–5, adversary tests |

The listener guard deserves the emphasis: tenet 5 is enforced fail-closed at
startup, not documented and hoped for. A configuration that would expose the
runtime without its safety conditions is a refusal to start, never a silently
degraded posture.

## 4. Implementation map

### The two invariants

Stated authoritatively at the top of [`runtime/src/lib.rs`](runtime/src/lib.rs);
neither may be quietly relaxed:

1. **The admin plane's boundary is the credential.** There is no admin socket,
   no UDS, no in-container CLI — no code path treats *being inside the
   container* as authorization. A managed shell shares the network namespace and
   *can* reach the listener; the claim is the weaker, true one: it cannot
   authenticate. This is why `adversary_linux.rs` asserts a `401`, **not** a
   refused connection.
2. **Teardown is best-effort.** Tier 1 (process group + subreaper/pidfd) is the
   only kill domain implemented. `kill_domain_tier = "tier2-required"` is
   refused at startup rather than downgraded — best effort must never be served
   under a guarantee's name.

### Startup order is load-bearing

`main.rs` is not a three-line `#[tokio::main]` because the order is the point:

1. `PR_SET_DUMPABLE=0`, fail-closed, **before any secret is read** — on Linux
   this is the only barrier between a same-UID session child and this process's
   `/proc/<pid>/mem`
2. Validate the delivered config projection, fail-closed — any evidence of an
   unresolved secret reference or broker config is a startup error
3. Kill-domain tier selection, logging what the tier does *not* promise
4. Seeded state into `$HOME`, before anything can observe the workspace
5. Bind, behind the listener guard

Graceful shutdown runs the reverse: notice → grace → `4006` → teardown.

### Module responsibilities

One line each; the authoritative doc comment is at the top of each file.

| Module | Responsibility | Key design point |
|---|---|---|
| `server.rs` | the single listening surface | attach is attach-only; admin is remote-only |
| `session.rs` | named byte-stream sessions | deliberately not the ACP pool's turn model |
| `token.rs` | attach-token storage | **no signing key exists** — a token is a random opaque capability whose hash is held in memory; eliminating the key eliminates the minting authority a same-container shell could steal |
| `admin_auth.rs` | admin credential verification | `sha256:` verifier only, constant-time compare; a success clears the source's throttle bucket, so a correct credential is never lockable |
| `killdomain.rs` | Tier 1 teardown | attribution (pidfd/subreaper) and termination (process group) solved separately |
| `ringbuf.rs` | output-path buffers | *every* buffer on the PTY→client path is bounded (ADR MUST); overflow surfaces as a `gap` control frame, never a sliced stream |
| `termfilter.rs` | capability-reply filtering | client→PTY direction **only**; PTY→client is verbatim by design (contract §4.1) |
| `containment.rs` | transient-secret containment | file permissions do not isolate same-UID processes; dumpable=0 is the barrier |
| `config.rs` | projection validation | this crate never resolves cloud secrets; the projection is materialized outside the PTY trust boundary |
| `seed.rs` | seeded agent state | extraction layer vendored from OpenAB; drift watched by `vendor-drift.yml` |
| `audit.rs` | structured audit events | fingerprints of hashes, never bearer plaintext |

### Lifecycle: the generation fence

`Generation` is a per-session monotonic counter bumped by renew, kill, and
restart-in-place. Every outstanding attach token for the session dies the moment
it changes — token revocation is a comparison, not a broadcast.

### Close codes are part of the design

Distinct codes `4001`–`4009` are a hard requirement, because a client that
reports "session expired" for a kill an operator just issued is telling its user
something untrue. The full table and required client behaviour are in
[contract §5](runtime/CLIENT-CONTRACT.md). The same honesty rule produces the
deliberate `401` ambiguity (expired token vs absent session — anti-enumeration)
and its resolution procedure in contract §6.

## 5. Honest status

Vision and current state, separated. "Delivered" means running and tested, not
planned.

| Capability | Status |
|---|---|
| Sandbox posture (uid 1000, no sudo, RO rootfs, no SA token) | **delivered** — SECURITY.md claims, adversary tests |
| K8s (k3s) and ECS Fargate deployment | **delivered** — both dogfooded, manifests in `deploy/` |
| EKS, GKE, OrbStack | **expected to work, unverified** — the runtime assumes nothing beyond a pod with a sidecar, but no one has run it there yet |
| Detach / re-attach with resume | **delivered** — with the stated boundary: sessions survive detach, **not pod replacement**; the workspace is ephemeral, which is why the attach notice says `externalise_with: git push` |
| Local-feel latency | **client-side by design** — the runtime's share is 1.0 ms; a client without echo prediction will feel sluggish no matter what this repo does |
| Single client controlling many distributed runtimes | **client-side** — the runtime is single-pod by scope; multi-runtime aggregation is the client's concern (contract §1 makes the client the operator) |
| Agent-to-agent communication | **not in this repository** — ACP/OpenAB territory; nothing here implements it, and nothing should |
| Tier 2 kill domain (per-session cgroup) | **not implemented** — demand-gated in the ADR; requesting it is a refused startup, not a silent downgrade |
| Web client, Helm chart | **Phase 1 non-goals** |

## 6. Non-goals

Things this repository deliberately does not do. A PR "helpfully" adding one of
these is reversing a decision, not filling a gap — reopen the discussion in the
ADR first:

- **No in-container admin path.** No UDS, no loopback-privileged CLI, no
  peer-credential check. The credential is the whole boundary (invariant 1).
- **No token signing key.** Tokens are opaque capabilities, not JWTs. A signing
  key would recreate the theft target the design removed.
- **No PTY→client sanitisation.** It is a terminal; stripping escape sequences
  breaks the programs people attach to it. The renderer's trust decision belongs
  to the client (contract §4.1).
- **No ACP, no brokering.** This runtime speaks PTY-over-WebSocket and nothing
  else. The turn-based world lives in `openabdev/openab`.
- **No teardown guarantees beyond Tier 1.** Best-effort is labelled as such
  everywhere it surfaces (invariant 2).
- **No trust in `X-Forwarded-For`.** Throttles key on the peer IP; the shipped
  topology's consequences are documented in SECURITY.md, not patched around.
- **No hard dependency between the pair.** `openab-pty` must remain useful as a
  standalone sandboxed terminal, and `openab` must not require it.
