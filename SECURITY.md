# Security Policy

## Reporting a vulnerability

**Do not open a public issue.** Use
[GitHub Security Advisories](https://github.com/openabdev/openab-pty/security/advisories/new),
which is private until we publish it.

Include what you did, what happened, and what you expected. A failing test or a
`curl` sequence is worth more than a description. If you are unsure whether
something counts, report it — the list below exists to save you time, not to
pre-empt your judgement.

Expect an acknowledgement within 5 working days. This is a small project with no
paid on-call, so that is a realistic figure rather than an SLA. We will tell you
when we have a fix, and credit you in the advisory unless you ask us not to.

## What this project claims

Only these are security claims. A report that breaks one of them is a
vulnerability:

1. **The admin plane's boundary is the credential.** A session shell can reach the
   listener over loopback — that is expected and is not a finding. The claim is
   that it cannot *authenticate to* or *successfully invoke* admin operations.
   Being inside the container is never treated as authorization; there is no
   in-container admin socket. A shell that manages sessions, its own or others', is
   a vulnerability.
2. **An attach token authorises exactly one session, and expires.** A token that
   attaches to a session it was not minted for, outlives its TTL, survives session
   revocation, or can be used to derive another token, is a vulnerability. Tokens
   carry no signing key by design.
3. **The admin credential is never recoverable from the runtime.** Only a
   `sha256:` hash is configured, compared in constant time. A path that leaks the
   credential, or that allows authentication without it, is a vulnerability.
4. **The shell holds no ambient authority.** `uid 1000`, no `sudo`, no
   service-account token, no host credentials, read-only root filesystem,
   ephemeral workspace. A path to `root`, to the host, to cloud credentials, or to
   a writable root filesystem is a vulnerability.
5. **The runtime does not listen on a routable address.** It binds loopback; the
   tailnet sidecar is the only network identity. A configuration that exposes the
   listener directly is a vulnerability.

## Known limits — please do not report these

These are documented design positions, not oversights. Reporting them costs you
time and gets a "working as intended" reply:

- **Teardown is best-effort.** Tier 1 is the only kill domain implemented. A
  process that leaves its process group may outlive its session until the pod or
  task is replaced. This is stated in the README and surfaced to clients.
- **A session shell can open a TCP connection to the listener.** See claim 1 —
  reachability is not the boundary, the credential is. The adversary test asserts
  `401`, not a refused connection, precisely because this is expected.
- **Anyone holding the admin credential controls every session.** That is what the
  credential is. Protecting it is the operator's job; the client keeps it in the
  system keychain.
- **The tailnet is trusted.** Anything on your tailnet can reach the sidecar.
  Scoping that is a tailnet ACL question, not a runtime one.
- **Third-party agent CLIs inside the published images.** Every variant except
  `native` bundles a vendor's CLI whose behaviour and vulnerabilities are the
  vendor's, not ours — see [`NOTICE`](NOTICE). Report those upstream. Issues in how
  *we* invoke or confine them are in scope.
- **`docs/` and `deploy/` use example account IDs and ARNs.** They are
  placeholders, not live infrastructure.

## Scope

In scope: the runtime under `runtime/`, the `Dockerfile`, and the manifests under
`deploy/`.

Out of scope: `OpenAB Connect` and any other client (report those to their own
maintainers), the `openabdev/openab` base image and the CLIs it bundles (report
upstream), and any deployment you operate yourself.

## Supported versions

Only the current `pre-beta` and `beta` channels receive fixes. This is Phase 1
software with no long-term support branch; there is no back-porting to older
image tags. Pull a current tag before reporting.
