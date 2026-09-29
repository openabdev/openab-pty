# openab-pty client contract

Implementation spec for a native client (macOS first, then iOS). Every shape
below was captured from the running Phase 1 runtime, not transcribed from the
source, so it reflects what a client will actually receive.

**Status:** implementation spec, not a tutorial. It describes the wire contract a
client must satisfy; it does not walk a user through using one.

---

## 1. What the client stores

```
connection profile = { base URL, admin credential }
```

Two values, in the Keychain. Everything else is derived: session names come from
the API, attach tokens are minted by the client, stream offsets are tracked in
memory.

This makes the client the **operator**, which is what the ADR intends by
"tokens are returned only to the external control client". It does not weaken the
sandbox: the container holds only a SHA-256 verifier, never the credential, so a
session child still has nothing to authenticate with.

**Two trust levels exist. Pick the second.**

| Level | Holds | Can do | Consequence |
|---|---|---|---|
| Attach-only | base URL + one attach token | attach to one named session until the token expires | dies at token expiry with no recovery path |
| **Operator** | base URL + admin credential | everything below, including minting its own tokens | self-sufficient |

Attach-only is why dogfooding kept stalling: expiry left no way back in without
an operator issuing a new token by hand.

## 2. Transport

- `wss://` wherever the deployment terminates TLS at an Ingress (the ADR's MVP
  default). Plain `ws://` is only defensible inside a WireGuard tailnet, which
  the runtime acknowledges by requiring `tls_terminated_upstream = true` before
  it will bind off-loopback at all.
- **Native clients send `Authorization: Bearer <token>`.** The
  `Sec-WebSocket-Protocol: openab.bearer.<token>` form exists solely because
  browsers cannot set headers on an upgrade. Do not use it.
- `Origin` is never consulted by the runtime. Possession of a valid attach token
  is the entire authorization, by decision, not oversight.

## 3. Admin API

All five require `Authorization: Bearer <admin-credential>`. Missing or wrong
credentials return `401` with an empty body and are counted against a per-source
failure throttle.

### `GET /admin/sessions`

```json
{
  "sessions": [
    { "name": "laptop", "generation": 1, "alive": true, "attached": false,
      "bytes_written": 28, "tier": "tier1-best-effort-process-group",
      "teardown_best_effort": true, "absolute_ttl_best_effort": true }
  ],
  "draining": false,
  "kill_domain": { "tier": "tier1-best-effort-process-group",
                   "teardown_best_effort": true, "absolute_ttl_best_effort": true,
                   "leaked_processes": 0, "tracked_processes": 3,
                   "tracked_process_ceiling": 512,
                   "subreaper": { "status": "active" } },
  "metrics": { "sessions_created": 4, "self_exits": 1, "ttl_expired": 0,
               "takeovers": 0, "takeovers_rate_limited": 0,
               "admission_rejected": 0 },
  "abuse": { "upgrade_failures": 5, "upgrade_bans": 0, "admin_auth_failures": 0,
             "malformed_frames": 0, "oversize_frames": 0,
             "input_backpressure_disconnects": 0 }
}
```

`teardown_best_effort` and `absolute_ttl_best_effort` are `true` under the
default kill domain (Tier 1). **Surface that honestly** — the ADR requires
best-effort semantics be labelled wherever they appear, and a client that implies
a hard guarantee is misreporting.

### `POST /admin/sessions` — body `{"name":"laptop"}`

```json
{ "session": "laptop", "generation": 1,
  "token": "<64 hex chars>", "token_expires_in_secs": 43199 }
```

Names are `[a-z0-9-]{1,32}`; anything else returns
`{"error":"invalid session name \"BAD NAME\": expected [a-z0-9-]{1,32}"}`.
Validate client-side so the user sees it before a round trip.

### `POST /admin/sessions/{name}/renew`

Same response shape. **The session process survives**; scrollback is kept. The
generation is bumped, so every outstanding token for that session dies at once,
and an attached connection is evicted with `4003`.

### `POST /admin/sessions/{name}/restart`

For reattach-to-dead: same name, fresh process, new generation, empty scrollback.

### `DELETE /admin/sessions/{name}`

Attached clients close with `4008`.

Errors are `{"error":"..."}`, e.g. `no such session: nosuch`,
`session capacity exceeded (limit 3)`.

## 4. Attach

```
GET /pty/{session}          Authorization: Bearer <attach-token>
GET /pty/{session}?since=N  resume from stream offset N
```

First text frame after upgrade:

```json
{ "v": 1, "type": "attach-notice", "stream_offset": 28,
  "ephemeral_workspace": true, "teardown_best_effort": true,
  "externalise_with": "git push (lifecycle hooks are backup, not primary)" }
```

- **Keep `stream_offset` and advance it by every payload byte received.** Pass it
  as `?since=` on reconnect to replay only what was missed. The first client
  ignored this and restarted from scratch every time.
- `ephemeral_workspace` must reach the user. The workspace does not survive pod
  replacement, and a terminal looks exactly like a local shell, so the assumption
  runs the other way unless stated.

Frames after that: **binary = PTY bytes**, **text = control JSON**.

Client → server control: `{"v":1,"type":"resize","cols":120,"rows":40}`,
`{"v":1,"type":"ping"}`, `{"v":1,"type":"detach"}`. Strict allowlist —
unknown types and out-of-range values count toward an abuse metric and
disconnect after three.

Server → client control: `gap` (with `bytes_dropped`, on ring-buffer overflow —
clear and redraw rather than rendering a sliced ANSI stream) and `ttl-warning`
(precedes forced teardown).

### 4.1 PTY output is not sanitised — that part is yours

Binary frames are the shell's bytes, **verbatim**, with one narrow exception.
The runtime answers *static* terminal-capability queries the child emits — Device
Attributes (DA1/DA2), DSR status, kitty-keyboard flags, and OSC 10/11 colour — at
the source, mirroring the reference client, and strips those queries from the
PTY → client stream so they never reach you or the ring buffer. This lets a
program negotiate even when no client is attached, and stops a `?since=` replay
from re-answering a query into a shell with no reader. Everything else is passed
through untouched — a runtime that stripped general escape sequences would break
the programs people attach to it.

**Cursor Position Report (`CSI 6 n`) is the exception to the exception.** The
runtime has no screen model, so it cannot answer CPR; it passes the query through
to you. If a program queries the cursor position, **your emulator must answer it**
(SwiftTerm does so automatically). Because CPR is not consumed at the source it
still rides the ring buffer, so a replay can re-deliver a CPR query — answer only
queries that arrive live, not those replayed from history, or the answer echoes
into the shell. (Answering CPR at the source awaits an embedded VT state machine;
see openabdev/openab-pty#15.)

The consequence for you: **the byte stream is untrusted input to your renderer.**
Whatever runs in that shell — an agent CLI, a compromised dependency, a file the
user `cat`s — chooses those bytes. Depending on what you feed them to, escape
sequences can set the window title, write the system clipboard (OSC 52), emit
hyperlinks (OSC 8), request a reply that your emulator will answer, or simply
desynchronise your parser.

So decide, explicitly rather than by default:

- Which OSC and DCS sequences you pass to your emulator. Clipboard writes and
  title changes are the ones with effects outside the terminal view.
- Whether your emulator answers queries, and if so that those replies go back over
  the socket as input — the runtime's filter exists precisely because they do.
- What you do with a `gap`: clear and redraw. Resuming mid-sequence hands your
  parser a truncated escape.

A client that pipes frames straight into a full-featured emulator inherits every
capability that emulator has. That is a client-side decision, and the runtime
cannot make it for you without ceasing to be a terminal.

## 5. Close codes

| Code | Meaning | Client action |
|---|---|---|
| 4001 | idle or absolute TTL elapsed | offer to create a new session |
| 4002 | another connection took over | offer to take it back; explain single-attach |
| 4003 | admin renewed the token | reconnect with the new token |
| 4004 | the shell exited | offer `restart` — **not** "expired" |
| 4005 | client too slow to drain | reconnect; consider a larger buffer |
| 4006 | runtime replaced (pod/task) | say the workspace was reset |
| 4007 | capacity or admission bound | show the limit; do not auto-retry hard |
| 4008 | operator killed it | say so plainly |
| 4009 | internal fault | do not invite retry-create |
| 4010 | tools grant revoked (tools socket only, §9) | the session lives; the lent Mac was withdrawn |
| **1006** | **the browser/WS layer never opened** | see below |

`1006` is not ours. It is what a client reports when the upgrade itself was
rejected, because the HTTP status is not visible at that layer. **Treat
"close 1006 with no prior open" as "the server refused the handshake".**

## 6. Disambiguating 401 — required, not optional

The runtime returns `401` for an expired token *and* for a session that does not
exist, deliberately, so session names cannot be enumerated. The two need
opposite responses, and guessing gets it wrong: during dogfooding a user was told
"your token expired" when the audit log showed
`SessionKill termination=Some(SelfExit)` — the shell had exited and the session
needed recreating.

On a rejected attach:

1. `GET /admin/sessions`
2. name present → the token is stale → `renew`, then retry attach silently
3. name absent → the session is gone → offer to create it, saying the previous
   shell exited

This keeps the anti-enumeration property in the runtime and puts the
disambiguation where the credential already is.

## 7. Two things that decide whether it feels good

Both are client-side. The runtime measured **1.0 ms** echo round-trip from its
own host, against **78–82 ms** from a laptop on WiFi, so nothing on the server
moves this number.

- **Local echo prediction.** Draw typed characters immediately, reconcile against
  the server echo. Without it, every character waits a full round trip. This is
  the largest single lever on perceived quality.
- **Keepalive while focused.** Latency was bimodal — a few samples at 4 ms among
  many at ~80 ms, with 0% packet loss and 41.5 ms of jitter — which is WiFi power
  saving, not distance. The protocol's own ping is 15–30 s, three orders of
  magnitude too slow to hold a radio awake. Ping every ~50 ms while the terminal
  has focus. Cheaper than prediction and it removes much of the same pain.

Do not attempt to fix either by changing the network path: the LAN path was
measured and was **not** better than the tailnet path. ICMP is not a proxy for
small-packet TCP over WiFi.

## 8. Minimum viable client

1. Store `{base URL, admin credential}` in the Keychain
2. List sessions
3. Create one if absent
4. Attach with `Authorization: Bearer`
5. Track `stream_offset`; reconnect with `?since=`
6. On `401`, disambiguate per §6 and recover without involving the user
7. Ping every ~50 ms while focused
8. Local echo prediction

Steps 1–6 are "works". Steps 7–8 are "feels good". Never silently swallow input
into a closed socket — that presented as data corruption when the real cause was
a takeover.

## 9. Tools attach — a Mac lends its tools to one session

This section is for a **different client**: not the terminal, but the machine
whose tools a coding CLI inside a session should be able to call — in practice
`oab-instance-mcp` on a Mac. Design of record:
[reverse attach](https://github.com/openabdev/instance-mcp/blob/main/docs/adr/reverse-attach.md).
The rule that shaped it: **the pod initiates nothing.** The Mac dials in, the pod
stores only a hash, and the CLI talks to a loopback URL with no credential in it.

```
  CLI in session S                  runtime                              Mac
  POST $OPENAB_TOOLS_MCP_URL  ──►  mux by JSON-RPC id  ◄── WS /tools/attach/S ◄── dials in
  (http://127.0.0.1:8091/mcp/S/<key>)                      Authorization: Bearer <secret>
```

Three parties, three credentials, none shared:

| Party | Holds | Gets it from |
|---|---|---|
| operator (Connect / Remote, or a curl) | admin credential | §1 |
| the Mac | attach **secret** for one session | operator, out of band, after `POST …/tools-attach` |
| the CLI in the session | per-session loopback **key**, inside the URL | its own environment, set at spawn |

The plane is off unless the deployment sets `tools_listen` (image env
`PTY_TOOLS_LISTEN`). Off means: no loopback listener, `/tools/attach` refuses
every upgrade with `401`, and the mint endpoint returns `501`.

### 9.1 Admin: mint and revoke

```
POST   /admin/sessions/{session}/tools-attach       Authorization: Bearer <admin-credential>
       optional JSON: {"ttl_secs": 14400}
DELETE /admin/sessions/{session}/tools-attach       Authorization: Bearer <admin-credential>
```

`POST` → `201`:

```json
{ "session": "laptop",
  "secret": "b1f0…64 lowercase hex…",
  "verifier": "sha256:…",
  "expires_in_secs": 14400,
  "ttl_secs": 14400,
  "attach": "/tools/attach/laptop" }
```

- `secret` is returned **once**. The runtime keeps only `verifier`. Hand the secret
  to the Mac; do not store it anywhere the runtime can see.
- Minting again **rotates** the secret and resets the TTL. A Mac already attached
  stays attached; a Mac that redials must present the new secret. This is how a
  grant is renewed: mint before expiry, hand over the new secret.
- Omit the body (or omit `ttl_secs`) for a **one-hour** grant. Connect/Remote
  request one of 1/2/4/12/24 hours by sending seconds. `tools_attach_ttl` is the
  operator's ceiling, default **24h** and itself hard-capped at 24h (env
  `PTY_TOOLS_ATTACH_TTL`); it is not the default lease. `ttl_secs = 0` or a request above that ceiling is `400` with an
  error naming the maximum — never a successful response with a silent shorter
  lease. The session's own absolute TTL can still end it first.
- `404` if the session does not exist; `501` if the plane is off.

`DELETE` → `204` always (once the name parses). It drops the grant and closes any
attached Mac with **`4010`**. Revoking nothing is not an error.

`GET /admin/sessions` gains a `tools` object keyed by session:

```json
"tools": { "laptop": { "granted": true, "grant_expires_in_secs": 3412,
                        "attached": true, "peer": "100.74.35.49:53102" } }
```

### 9.2 The Mac: `GET /tools/attach/{session}`

WebSocket upgrade with `Authorization: Bearer <secret>`. Same anti-enumeration rule as `/pty`: a missing session,
a wrong secret, an expired grant and a disabled plane are all an indistinguishable
`401`, counted against the same per-source upgrade throttle (`429` after five in
a minute).

Once upgraded, **the Mac is the MCP server and the runtime is its MCP client.**
Text frames carry plain MCP JSON-RPC, no envelope:

1. The runtime sends `initialize` (`protocolVersion` `2025-06-18`, `clientInfo`
   `openab-pty`). Answer it; the `serverInfo` you return is what `instance_status`
   reports to the CLI. The runtime then sends `notifications/initialized`.
2. Every `tools/list`, `tools/call` and any other request the CLI issues arrives
   with a runtime-assigned integer `id`. Reply with the same `id`. Ids are
   rewritten on both sides; never assume they match the CLI's.
3. The runtime pings every 20 s and closes after three intervals with no inbound
   frame. Answer WS pings (any library does); nothing else is required.
4. Notifications you send (e.g. `notifications/tools/list_changed`) are accepted
   and dropped — there is no push channel toward the CLI. Requests you send are
   answered `-32601` except `ping`.
5. Frames up to 16 MiB are accepted from the Mac (screenshots are large). The
   runtime holds at most 64 requests in flight per session and answers the CLI
   with a JSON-RPC error after 120 s without your reply; you are not told.

**One Mac per session.** A second upgrade for the same session **replaces** the
first, which is closed with `4002`. A dialer that redials on disconnect therefore
converges instead of fighting an incumbent; this is deliberate, and the reason is
in the 4a spike (openabdev/openab-pty#37): two dialers on one session produced 418
failed attaches in minutes.

Close codes you will see on this socket:

| Code | Meaning | Dialer action |
|---|---|---|
| 4001 | the grant's TTL elapsed | stop; a human must mint again |
| 4002 | replaced by a newer attach for this session | stop; the newer one is you or a peer |
| 4004 | the session ended (killed, exited, expired) | stop; nothing to attach to |
| 4006 | runtime replaced (pod/task) | back off, redial while the grant should still be valid; the verifier is gone with the pod, so expect `401` until re-minted |
| 4010 | grant revoked by the admin plane | stop |
| 1000 | you closed, or the socket dropped | redial with backoff while the grant is valid |

Retry ownership is yours: redial for the remainder of the grant, with backoff,
and give up on any 4xxx except 4006. The pod cannot reach you.

### 9.3 The CLI: `$OPENAB_TOOLS_MCP_URL`

Every session child is spawned with

```
OPENAB_TOOLS_MCP_URL=http://127.0.0.1:<tools port>/mcp/<session>/<64-hex key>
OPENAB_TOOLS_MCP_ENDPOINT=http://127.0.0.1:<tools port>/mcp
OPENAB_TOOLS_MCP_TOKEN=<the same 64-hex key>
```

Two equivalent ways in, to the same session's tools:

- `POST $OPENAB_TOOLS_MCP_URL` — the session is in the path.
- `POST $OPENAB_TOOLS_MCP_ENDPOINT` with `Authorization: Bearer $OPENAB_TOOLS_MCP_TOKEN` —
  the endpoint is **identical in every session**; the key selects the session.
  Missing, malformed or unknown key → `401` (`WWW-Authenticate: Bearer`),
  indistinguishable. Use this form whenever the CLI's MCP config is a file shared
  by several sessions (a workspace `.kiro/settings/mcp.json`, a repo `.mcp.json`):
  such a file cannot name one session in its URL, and a URL pinned to one session
  silently routes every session to that session's computer.

Point the CLI's MCP config at one of them as a Streamable HTTP server. The runtime
does not edit any CLI's config; whatever installs the CLI does that. Properties:

- `POST` one JSON-RPC object → one JSON response, `200`. A notification → `202`,
  empty body. `GET` → `405`: there is no server-to-client stream, and saying so
  beats holding an SSE connection that never carries a frame.
- Wrong or missing key, or unknown session → `404`, indistinguishable, so a caller
  cannot enumerate sessions. `GET /healthz` → `ok`.
- `initialize` is answered **locally** by the runtime (`serverInfo.name`
  `openab-pty-tools`), attached or not, with `instructions` that say which.
- `tools/list` returns the Mac's tools **plus** `instance_status`, a tool the
  runtime answers itself. When nothing is attached the list is exactly
  `[instance_status]` — not empty, not an error — so an agent can tell "no hands
  were lent" from "the endpoint is broken".
- `tools/call instance_status` →
  `{"attached": true, "peer": …, "server": {serverInfo}, "grant_expires_in_secs": …}`
  or `{"attached": false, "grant_pending": bool}`.
- `tools/call <anything else>` while not attached → a **tool** result with
  `isError: true` and a message telling the agent to ask the human, rather than a
  protocol error that would make an MCP client tear the server down.
- Mac unreachable mid-call → JSON-RPC error `-32001` (not attached), `-32002`
  (too many in flight), `-32003` (timeout). The socket is torn down and the next
  `tools/list` is back to `[instance_status]`.
- The key is the session's; a restart-in-place spawns a new shell with a new key,
  and the old one stops working. The listener is loopback-bound and the runtime
  refuses to start otherwise — but it **is** reachable from the tailnet through
  the sidecar like every other loopback port, which is why the key exists.

#### Wiring the URL into the coding CLI

The runtime sets the env vars; it does **not** edit the CLI's config. Whatever owns
the session's workspace does that.

**Preferred — one config for every session.** The file never changes, across
sessions, restarts or re-lends, as long as the CLI expands environment variables in
HTTP headers. `kiro-cli` does (`${VAR}` in `headers`; it does **not** expand `url`):

```json
{
  "mcpServers": {
    "computer": {
      "url": "http://127.0.0.1:<tools port>/mcp",
      "headers": { "Authorization": "Bearer ${OPENAB_TOOLS_MCP_TOKEN}" }
    }
  }
}
```

The port is fixed by `PTY_TOOLS_LISTEN`, so the URL is a constant. Each CLI process
expands `${OPENAB_TOOLS_MCP_TOKEN}` from its own session's environment.

**Per session — only when the config is private to one session.** For `kiro-cli`
(2.13+), inside the session shell:

```sh
kiro-cli mcp add --name computer --url "$OPENAB_TOOLS_MCP_URL" --scope global
```

`mcp add` registers the server but does not trust its tools, so each call prompts.
To pre-trust them, list them in the agent config's `allowedTools` under the
`@<server>/<tool>` naming — for the `computer` server,
`@computer/screenshot`, `@computer/browser_navigate`, … A
`~/.kiro/agents/<agent>.json` with both the server and
the allowlist means the agent starts with the Mac's tools already usable:

```json
{
  "name": "kiro_default",
  "mcpServers": { "computer": { "url": "http://127.0.0.1:<port>/mcp/<session>/<key>" } },
  "allowedTools": ["@computer/screenshot", "@computer/mouse", "@computer/key", "@computer/osascript",
                   "@computer/browser_navigate", "@computer/browser_snapshot", "@computer/browser_evaluate", "…"]
}
```

Alternatively, per invocation:
`kiro-cli chat --trust-tools=@computer/screenshot,@computer/browser_navigate,…`.
Other CLIs use their own MCP config shape; the URL is the same plain loopback URL.

The URL path remains `/mcp/<session>/<key>`. A live URL such as
`/mcp/mac/606e…` means the **PTY session is named `mac`**; it is not the MCP
server alias and must not be rewritten to `/mcp/computer/…`. A session named
`work` gets `/mcp/work/<key>` while the Kiro alias stays `computer`.

Migration from the pre-platform-neutral alias:

```sh
kiro-cli mcp remove --name mac
kiro-cli mcp add --name computer --url "$OPENAB_TOOLS_MCP_URL" --scope global
```

Update `@mac/*` entries in an agent's `allowedTools` to `@computer/*` at the same
time; keeping both aliases duplicates every served tool.

**Caveat — the key rotates** (this is what the header form above avoids). The `<key>` in `$OPENAB_TOOLS_MCP_URL` is per session
*generation*: a restart-in-place, or tearing down and re-lending a Mac, mints a new
URL, and any config that hard-codes the old one (both `mcp.json` and the agent's
`allowedTools` server entry) must be updated. Re-run `mcp add`, or read
`$OPENAB_TOOLS_MCP_URL` again, after each re-lend. Injecting and refreshing this
automatically at session spawn is [tracked as #39](https://github.com/openabdev/openab-pty/issues/39); until then it is a documented manual step.

### 9.4 Minimum viable lender

1. Operator: `POST /admin/sessions/{s}/tools-attach`, hand `secret` to the Mac.
2. Mac: open `ws(s)://<runtime>/tools/attach/{s}` with `Authorization: Bearer <secret>`.
3. Mac: answer `initialize`, then serve `tools/list` / `tools/call` as an MCP server.
4. Mac: on close 1000/4006 redial with backoff while the grant is valid; on any
   other 4xxx stop.
5. Operator: `DELETE …/tools-attach` to withdraw; `POST` again before expiry to renew.

The CLI side is zero steps: the URL is already in its environment.
