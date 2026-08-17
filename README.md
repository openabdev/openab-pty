# oab-pty-mac

Prototype native client for `openab-pty`. Implements the contract in
`openab/crates/openab-pty/CLIENT-CONTRACT.md`.

Why native rather than a browser page, in the order the reasons were discovered
by using the thing:

- **Keychain.** The runtime's attach tokens are short-lived and the ADR forbids a
  browser persisting them. Holding the *admin credential* locally lets this app
  renew its own tokens, which is what stops the "ask someone for a new token
  every few minutes" loop.
- **A header.** Native clients send `Authorization: Bearer`; the
  `Sec-WebSocket-Protocol: openab.bearer.<token>` form exists only because
  browsers cannot.
- **401 is ambiguous by design.** The runtime answers 401 for both an expired
  token and a missing session, so names cannot be enumerated. This client asks
  the admin API which one it is and recovers accordingly, instead of guessing.
- **Keepalive.** Measured echo latency was bimodal (a few samples at 4 ms among
  many at ~80 ms, 0% packet loss, 41.5 ms jitter) — WiFi power saving, not
  distance. The protocol's own ping is 15–30 s, far too slow to hold a radio
  awake, so this client pings every ~50 ms.

## Build

Not built locally: `swift build -c release` on the build host, then the binary is
wrapped in a minimal `.app` so it gets a Dock presence.

```
rsync -a --exclude .build ./ macmini:~/build/oab-pty-mac/
ssh macmini 'cd ~/build/oab-pty-mac && swift build -c release'
scp macmini:~/build/oab-pty-mac/.build/release/OabPtyMac dist/
```

## Not implemented

**Local echo prediction.** The largest remaining lever on perceived quality: the
runtime measured 1.0 ms from its own host against 78–82 ms from a laptop, so with
no prediction every character waits a full round trip. Deliberately deferred —
it is the hard part, and everything else needed to be working first.
