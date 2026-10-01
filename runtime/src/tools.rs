//! Reverse-attached tools: a Mac lends its `oab-instance-mcp` tools to one PTY
//! session by **dialling in**, and the coding CLI inside that session reaches
//! them over a plain loopback MCP endpoint.
//!
//! Design of record: `docs/adr/reverse-attach.md` in `openabdev/instance-mcp`;
//! runtime issue openabdev/openab-pty#37. The shape, and why each half is where
//! it is:
//!
//! ```text
//!   CLI in session S                 this runtime                         Mac
//!   POST 127.0.0.1:<tools>/mcp/S/<k> ──► ToolsHub ──── WS /tools/attach/S ◄── dials in,
//!   no token · no TLS · no proxy         mux by id     sha256 verifier only    holds the secret
//! ```
//!
//! Three properties are enforced here by type and by check, not by convention:
//!
//! 1. **The pod never holds a usable secret.** The admin plane mints an attach
//!    secret, returns it once, and the hub keeps only its SHA-256. A same-UID
//!    shell that reads this process's memory learns a verifier, which connects
//!    to nothing. Same model as [`crate::token`].
//! 2. **The loopback listener is loopback.** [`crate::config`] refuses a
//!    non-loopback `tools_listen`, and the bind path checks the *bound* address
//!    again, so a misconfiguration cannot turn the unauthenticated MCP surface
//!    into a network service.
//! 3. **Exactly one Mac per session, and the runtime decides.** A second attach
//!    for the same session replaces the first with [`close_code::TAKEOVER`]; a
//!    dialer that keeps redialling therefore converges instead of fighting the
//!    incumbent for a port. The 4a spike measured the alternative: 418 failed
//!    attaches and a `TIME_WAIT` storm in a few minutes.
//!
//! The loopback surface carries no credential beyond a **per-session key** that
//! lives only in that session's child environment (`OPENAB_TOOLS_MCP_URL`). Two
//! facts decide how much that key has to do:
//!
//! - "Loopback" is not "unreachable". The userspace tailscale sidecar forwards
//!   inbound tailnet TCP to *every* loopback port of the pod (measured in the 4a
//!   spike), so this listener is reachable by any tailnet peer, exactly like the
//!   admin listener. The key is therefore the whole boundary against a tailnet
//!   peer, and a wrong or missing key is an indistinguishable `404`.
//! - As everywhere in this runtime, two sessions run as one UID. A shell in A
//!   that reads B's `/proc/<pid>/environ` has already crossed a line this runtime
//!   does not claim to hold. The key stops guessing and stops neighbours on the
//!   tailnet; it does not stop a same-UID neighbour in the pod.
//!
//! The runtime is the MCP *client* to the Mac (it sends `initialize` on attach)
//! and the MCP *server* to the CLI (it answers the CLI's `initialize` itself).
//! Everything else is forwarded with the JSON-RPC `id` rewritten, so several
//! loopback callers can share one socket without colliding.

use crate::audit::{hash_fingerprint, AuditEvent, AuditKind, AuditLogger};
use crate::close_code;
use crate::containment::SecretBytes;
use crate::{Error, SessionName};
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot};

/// Default lifetime when the mint request omits `ttl_secs`.
pub const DEFAULT_TOOLS_ATTACH_TTL: Duration = Duration::from_secs(60 * 60);
/// Default configured upper bound. Connect/Remote offer up to 24 hours; the
/// operator can lower this with `[pty].tools_attach_ttl` / `PTY_TOOLS_ATTACH_TTL`.
pub const DEFAULT_TOOLS_ATTACH_MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Bound on requests a session may have in flight toward its Mac. Past it the
/// loopback caller gets a JSON-RPC error immediately rather than queueing.
pub const MAX_INFLIGHT_PER_SESSION: usize = 64;
/// How long a forwarded request may wait for the Mac before the loopback caller
/// is told so. Screenshots and AppleScript can be slow; a hung Mac must not be
/// a hung CLI.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest JSON-RPC body accepted from the loopback side.
pub const MAX_LOOPBACK_BODY_BYTES: usize = 1024 * 1024;
/// Largest WS message accepted from the Mac. Screenshot results are base64 PNGs
/// of a whole display, so this is deliberately generous.
pub const MAX_MAC_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Liveness: the runtime pings the Mac at this interval and closes after
/// [`MAX_MISSED_PINGS`] intervals with no inbound frame of any kind.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);
pub const MAX_MISSED_PINGS: u32 = 3;
/// The MCP protocol revision this runtime speaks to both sides.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// Name of the tool the runtime answers itself, attached or not.
pub const STATUS_TOOL: &str = "instance_status";
/// Outbound-frame queue toward the Mac.
const TO_MAC_QUEUE: usize = 64;

// ---------------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Grant {
    hash: [u8; 32],
    expires_at: Instant,
}

/// One-time result of minting. The plaintext is returned to the admin caller and
/// is not retained.
pub struct MintedToolsAttach {
    pub plaintext: SecretBytes,
    pub verifier: String,
    pub expires_at: Instant,
}

// ---------------------------------------------------------------------------
// Attached socket
// ---------------------------------------------------------------------------

/// One reverse-attached Mac. Dropped from the hub when its socket closes.
pub struct Attached {
    session: SessionName,
    peer: String,
    since: Instant,
    to_mac: mpsc::Sender<Message>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    close_code: AtomicU64,
    server_info: Mutex<Option<Value>>,
}

impl Attached {
    /// Ask the socket to close with `code`. Idempotent: the first code wins, so a
    /// lifecycle reason chosen by the hub (revoke, takeover, TTL) is never
    /// overwritten by the generic fallback the socket task applies on exit.
    fn close(&self, code: u16) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.close_code.store(u64::from(code), Ordering::Release);
        let _ = self.to_mac.try_send(Message::Close(Some(CloseFrame {
            code,
            reason: close_reason(code).into(),
        })));
        self.fail_pending();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn fail_pending(&self) {
        let drained: Vec<_> = self.pending.lock().drain().collect();
        for (_, waiter) in drained {
            // A dropped sender is how the awaiting caller learns the socket is
            // gone; it maps that to a JSON-RPC error itself.
            drop(waiter);
        }
    }

    /// Forward one request to the Mac and wait for its response, with the id
    /// rewritten so concurrent loopback callers cannot collide.
    async fn call(&self, mut request: Value) -> Result<Value, CallError> {
        if self.is_closed() {
            return Err(CallError::NotAttached);
        }
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut pending = self.pending.lock();
            if pending.len() >= MAX_INFLIGHT_PER_SESSION {
                return Err(CallError::TooManyInFlight);
            }
            let id = self.next_id.fetch_add(1, Ordering::AcqRel);
            pending.insert(id, tx);
            id
        };
        if let Some(object) = request.as_object_mut() {
            object.insert("id".into(), json!(id));
        }
        if self
            .to_mac
            .send(Message::Text(request.to_string().into()))
            .await
            .is_err()
        {
            self.pending.lock().remove(&id);
            return Err(CallError::NotAttached);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(CallError::NotAttached),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(CallError::Timeout)
            }
        }
    }

    /// Route a frame from the Mac: a response completes a pending call; a
    /// request from the Mac is answered minimally; notifications are dropped
    /// (there is no push channel toward the CLI).
    fn on_mac_frame(&self, frame: Value) {
        let is_response = frame.get("result").is_some() || frame.get("error").is_some();
        if is_response {
            let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                return;
            };
            if let Some(waiter) = self.pending.lock().remove(&id) {
                let _ = waiter.send(frame);
            }
            return;
        }
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return;
        };
        let Some(id) = frame.get("id").cloned() else {
            // Notification from the Mac (e.g. tools/list_changed). Nothing to
            // deliver it to.
            return;
        };
        let reply = match method {
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => rpc_error(id, -32601, "method not supported by the openab-pty mux"),
        };
        let _ = self
            .to_mac
            .try_send(Message::Text(reply.to_string().into()));
    }

    fn status(&self, grant_expires_at: Option<Instant>) -> Value {
        json!({
            "attached": true,
            "session": self.session.as_str(),
            "peer": self.peer,
            "attached_for_secs": self.since.elapsed().as_secs(),
            "in_flight": self.pending.lock().len(),
            "server": self.server_info.lock().clone(),
            "grant_expires_in_secs": grant_expires_at
                .map(|at| at.saturating_duration_since(Instant::now()).as_secs()),
        })
    }
}

/// Why a forwarded call did not produce a response from the Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    NotAttached,
    TooManyInFlight,
    Timeout,
}

impl CallError {
    fn rpc_code(self) -> i64 {
        match self {
            // Implementation-defined server errors, in the JSON-RPC reserved range.
            Self::NotAttached => -32001,
            Self::TooManyInFlight => -32002,
            Self::Timeout => -32003,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::NotAttached => {
                "no Mac is attached to this session (ask the human to lend one from OpenAB Connect / Remote)"
            }
            Self::TooManyInFlight => "too many requests in flight toward the attached Mac",
            Self::Timeout => "the attached Mac did not answer in time",
        }
    }
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// Per-session grants, loopback keys and live attaches.
///
/// Held by the server state. The admin routes mint and revoke; the attach route
/// verifies and installs sockets; the loopback route calls; the ticker sweeps.
pub struct ToolsHub {
    grants: Mutex<HashMap<SessionName, Grant>>,
    /// Random per-session path component for the loopback endpoint. Created
    /// when a session's child is spawned, so it exists before the CLI can ask.
    loopback_keys: Mutex<HashMap<SessionName, [u8; 32]>>,
    attached: Mutex<HashMap<SessionName, Arc<Attached>>>,
    max_ttl: Duration,
    audit: AuditLogger,
}

impl ToolsHub {
    /// `max_ttl` is an operator ceiling, not the default lease. Requests that
    /// omit `ttl_secs` remain one hour for backwards compatibility.
    pub fn new(max_ttl: Duration, audit: AuditLogger) -> Self {
        Self {
            grants: Mutex::new(HashMap::new()),
            loopback_keys: Mutex::new(HashMap::new()),
            attached: Mutex::new(HashMap::new()),
            max_ttl,
            audit,
        }
    }

    pub fn max_ttl(&self) -> Duration {
        self.max_ttl
    }

    // -- grants -------------------------------------------------------------

    /// Mint (or rotate) the attach secret for a session. A live attach, if any,
    /// keeps its socket: renewal is "new secret, same connection", so a Mac that
    /// redials after the old secret expires is not evicted mid-grant.
    pub fn mint(&self, session: &SessionName) -> Result<MintedToolsAttach, Error> {
        self.mint_with_ttl(session, DEFAULT_TOOLS_ATTACH_TTL.min(self.max_ttl))
    }

    /// Mint with the admin caller's requested lifetime. Refuse rather than cap:
    /// a silent cap is exactly how a 12-hour Connect lease used to become one
    /// hour while every layer reported success.
    pub fn mint_with_ttl(
        &self,
        session: &SessionName,
        ttl: Duration,
    ) -> Result<MintedToolsAttach, Error> {
        if ttl.is_zero() {
            return Err(Error::Other("tools attach TTL must be non-zero".into()));
        }
        if ttl > self.max_ttl {
            return Err(Error::Other(format!(
                "tools attach TTL exceeds configured maximum of {} seconds",
                self.max_ttl.as_secs()
            )));
        }
        let encoded = random_hex()?;
        let hash = sha256(&encoded);
        let expires_at = Instant::now() + ttl;
        self.grants
            .lock()
            .insert(session.clone(), Grant { hash, expires_at });
        self.audit.record(
            AuditEvent::new(AuditKind::ToolsGrantMinted)
                .session_name(session)
                .fingerprint(hash_fingerprint(&hash))
                .detail(format!("ttl_secs={}", ttl.as_secs())),
        );
        Ok(MintedToolsAttach {
            plaintext: SecretBytes::new(encoded),
            verifier: format!("sha256:{}", hex::encode(hash)),
            expires_at,
        })
    }

    /// Drop the grant and close any live attach with `code`. Returns whether
    /// there was anything to revoke.
    pub fn revoke(&self, session: &SessionName, code: u16) -> bool {
        let grant = self.grants.lock().remove(session);
        let attached = self.attached.lock().remove(session);
        if let Some(grant) = &grant {
            self.audit.record(
                AuditEvent::new(AuditKind::ToolsGrantRevoked)
                    .session_name(session)
                    .fingerprint(hash_fingerprint(&grant.hash))
                    .detail(close_reason(code)),
            );
        }
        if let Some(attached) = &attached {
            attached.close(code);
        }
        grant.is_some() || attached.is_some()
    }

    /// Constant-time verification of a presented attach secret. Erases the
    /// presented bytes before returning, as [`crate::token`] does.
    pub fn verify(
        &self,
        session: &SessionName,
        presented: &mut SecretBytes,
        source: &str,
    ) -> Result<(), Error> {
        let presented_hash = sha256(presented.as_bytes());
        presented.zeroize();
        let now = Instant::now();
        let mut grants = self.grants.lock();
        let ok = grants.get(session).is_some_and(|grant| {
            grant.expires_at > now && bool::from(presented_hash.ct_eq(&grant.hash))
        });
        if ok {
            return Ok(());
        }
        if grants
            .get(session)
            .is_some_and(|grant| grant.expires_at <= now)
        {
            grants.remove(session);
        }
        drop(grants);
        self.audit.record(
            AuditEvent::new(AuditKind::ToolsAuthFailure)
                .session_name(session)
                .source(source)
                .detail("invalid_or_expired_tools_attach_secret"),
        );
        Err(Error::Unauthorized)
    }

    fn grant_expiry(&self, session: &SessionName) -> Option<Instant> {
        self.grants
            .lock()
            .get(session)
            .map(|grant| grant.expires_at)
    }

    // -- loopback keys --------------------------------------------------------

    /// Fresh random key for a session's loopback URL. Called by the session
    /// manager at spawn; the previous key (if any) stops working, which is right:
    /// the old shell is gone.
    pub fn issue_loopback_key(&self, session: &SessionName) -> String {
        let mut raw = [0u8; 32];
        // A failure here is not worth refusing the spawn over; a zero key simply
        // never matches, so the session has no tools until a restart.
        if getrandom::fill(&mut raw).is_err() {
            tracing::warn!(%session, "OS RNG failed while issuing a tools loopback key");
            self.loopback_keys.lock().remove(session);
            return String::new();
        }
        self.loopback_keys.lock().insert(session.clone(), raw);
        hex::encode(raw)
    }

    /// Whether `presented` is the current loopback key for `session`.
    pub fn check_loopback_key(&self, session: &SessionName, presented: &str) -> bool {
        let Ok(bytes) = hex::decode(presented) else {
            return false;
        };
        let Some(expected) = self.loopback_keys.lock().get(session).copied() else {
            return false;
        };
        bytes.len() == expected.len() && bool::from(bytes.as_slice().ct_eq(&expected))
    }

    /// The session whose current loopback key is `presented`, for the
    /// session-independent `POST /mcp` + `Authorization: Bearer` route. Every
    /// key is compared in constant time and the scan never exits early, so the
    /// answer's timing does not depend on which session (if any) matched.
    pub fn session_for_loopback_key(&self, presented: &str) -> Option<SessionName> {
        let bytes = hex::decode(presented).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let keys = self.loopback_keys.lock();
        let mut found: Option<SessionName> = None;
        for (session, expected) in keys.iter() {
            if bool::from(bytes.as_slice().ct_eq(expected)) {
                found = Some(session.clone());
            }
        }
        found
    }

    pub fn forget_session(&self, session: &SessionName) {
        self.loopback_keys.lock().remove(session);
        self.revoke(session, close_code::SESSION_ENDED);
    }

    // -- attach ----------------------------------------------------------------

    /// Install a verified socket as the session's Mac. Any incumbent is evicted
    /// with `TAKEOVER`. Runs until the socket closes.
    pub async fn serve_attached(
        self: &Arc<Self>,
        session: SessionName,
        socket: WebSocket,
        peer: String,
    ) {
        let (to_mac, mut from_hub) = mpsc::channel::<Message>(TO_MAC_QUEUE);
        let attached = Arc::new(Attached {
            session: session.clone(),
            peer: peer.clone(),
            since: Instant::now(),
            to_mac,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            close_code: AtomicU64::new(0),
            server_info: Mutex::new(None),
        });
        let evicted = self
            .attached
            .lock()
            .insert(session.clone(), attached.clone());
        if let Some(incumbent) = evicted {
            tracing::info!(%session, "tools attach replaced an incumbent Mac");
            incumbent.close(close_code::TAKEOVER);
        }
        self.audit.record(
            AuditEvent::new(AuditKind::ToolsAttach)
                .session_name(&session)
                .source(&peer),
        );

        let (mut sink, mut stream) = socket.split();
        let writer = tokio::spawn(async move {
            while let Some(message) = from_hub.recv().await {
                let closing = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() {
                    break;
                }
                if closing {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // The runtime is the MCP client on this socket: handshake with the Mac so
        // its server info is known before the first CLI call arrives.
        let handshake = attached.clone();
        tokio::spawn(async move {
            let init = json!({
                "jsonrpc": "2.0",
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "openab-pty", "version": env!("CARGO_PKG_VERSION") }
                }
            });
            match handshake.call(init).await {
                Ok(response) => {
                    *handshake.server_info.lock() = response
                        .get("result")
                        .and_then(|r| r.get("serverInfo"))
                        .cloned();
                    let initialized =
                        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
                    let _ = handshake
                        .to_mac
                        .send(Message::Text(initialized.to_string().into()))
                        .await;
                }
                Err(error) => {
                    tracing::warn!(session = %handshake.session, ?error, "MCP initialize toward the Mac failed");
                }
            }
        });

        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await;
        let mut missed: u32 = 0;
        loop {
            tokio::select! {
                frame = stream.next() => {
                    let Some(Ok(frame)) = frame else { break };
                    missed = 0;
                    match frame {
                        Message::Text(text) => match serde_json::from_str::<Value>(&text) {
                            Ok(value) => attached.on_mac_frame(value),
                            Err(error) => tracing::debug!(%session, %error, "non-JSON text frame from the Mac"),
                        },
                        Message::Binary(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                            Ok(value) => attached.on_mac_frame(value),
                            Err(error) => tracing::debug!(%session, %error, "non-JSON binary frame from the Mac"),
                        },
                        Message::Ping(payload) => {
                            let _ = attached.to_mac.try_send(Message::Pong(payload));
                        }
                        Message::Pong(_) => {}
                        Message::Close(_) => break,
                    }
                }
                _ = ping.tick() => {
                    if attached.is_closed() { break; }
                    missed += 1;
                    if missed > MAX_MISSED_PINGS {
                        tracing::info!(%session, "tools attach: Mac silent past the ping budget");
                        break;
                    }
                    let _ = attached.to_mac.try_send(Message::Ping(Vec::new().into()));
                }
            }
            if attached.is_closed() {
                break;
            }
        }

        // Socket gone (or we decided it is). Normal closure unless a lifecycle
        // code was already chosen; then detach from the hub if still current.
        attached.close(1000);
        {
            let mut map = self.attached.lock();
            if map
                .get(&session)
                .is_some_and(|current| Arc::ptr_eq(current, &attached))
            {
                map.remove(&session);
            }
        }
        let _ = writer.await;
        let code = attached.close_code.load(Ordering::Acquire) as u16;
        self.audit.record(
            AuditEvent::new(AuditKind::ToolsDetach)
                .session_name(&session)
                .source(&peer)
                .detail(close_reason(code)),
        );
    }

    fn attached_for(&self, session: &SessionName) -> Option<Arc<Attached>> {
        self.attached
            .lock()
            .get(session)
            .filter(|attached| !attached.is_closed())
            .cloned()
    }

    // -- loopback surface -------------------------------------------------------

    /// Answer one JSON-RPC message from the CLI. `None` means "notification,
    /// nothing to return".
    pub async fn handle_loopback(&self, session: &SessionName, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(id) = id else {
            // Notification. `notifications/initialized` and friends are between
            // the CLI and us; the Mac has already been initialised by the hub.
            return None;
        };
        match method.as_str() {
            "initialize" => Some(self.local_initialize(session, id, &request)),
            "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            "tools/list" => Some(self.tools_list(session, id, request).await),
            "tools/call" => Some(self.tools_call(session, id, request).await),
            _ => match self.attached_for(session) {
                Some(attached) => Some(forward(&attached, id, request).await),
                None => Some(rpc_error(
                    id,
                    -32601,
                    "method not available: no Mac is attached to this session",
                )),
            },
        }
    }

    fn local_initialize(&self, session: &SessionName, id: Value, request: &Value) -> Value {
        // Echo a version the client offered when it is one we know; otherwise
        // state ours and let the client decide.
        let offered = request
            .get("params")
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or(MCP_PROTOCOL_VERSION);
        let version = if offered == MCP_PROTOCOL_VERSION || offered == "2025-03-26" {
            offered
        } else {
            MCP_PROTOCOL_VERSION
        };
        let attached = self.attached_for(session).is_some();
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "openab-pty-tools", "version": env!("CARGO_PKG_VERSION") },
                "instructions": if attached {
                    "A Mac is currently lent to this session. Its tools are listed under tools/list alongside instance_status. The grant is time-bounded; call instance_status if a tool starts failing."
                } else {
                    "No Mac is attached to this session right now. Only instance_status is available; ask the human to lend a Mac from OpenAB Connect or Remote, then call tools/list again."
                }
            }
        })
    }

    async fn tools_list(&self, session: &SessionName, id: Value, request: Value) -> Value {
        let status_tool = json!({
            "name": STATUS_TOOL,
            "description": "Whether a Mac is attached to this PTY session through openab-pty, and for how long the grant lasts. Answered by the runtime itself; works even when nothing is attached.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        });
        let Some(attached) = self.attached_for(session) else {
            return json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [status_tool] } });
        };
        let mut response = forward(&attached, id.clone(), request).await;
        match response
            .get_mut("result")
            .and_then(|r| r.get_mut("tools"))
            .and_then(Value::as_array_mut)
        {
            Some(tools) => {
                tools.retain(|tool| tool.get("name").and_then(Value::as_str) != Some(STATUS_TOOL));
                tools.push(status_tool);
                response
            }
            // Error from the Mac, or an unexpected shape: still surface the local
            // tool so the agent can ask what is going on.
            None => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [status_tool] } }),
        }
    }

    async fn tools_call(&self, session: &SessionName, id: Value, request: Value) -> Value {
        let name = request
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name == STATUS_TOOL {
            let status = match self.attached_for(session) {
                Some(attached) => attached.status(self.grant_expiry(session)),
                None => json!({
                    "attached": false,
                    "session": session.as_str(),
                    "grant_pending": self.grant_expiry(session).is_some(),
                }),
            };
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": status.to_string() }],
                    "structuredContent": status,
                    "isError": false
                }
            });
        }
        match self.attached_for(session) {
            Some(attached) => forward(&attached, id, request).await,
            // A tool error, not a protocol error: the agent reads the text and
            // can tell the human, instead of its MCP client tearing down.
            None => tool_error_result(id, CallError::NotAttached.message()),
        }
    }

    // -- maintenance ---------------------------------------------------------

    /// Expire grants and close orphaned attaches. `session_exists` is asked
    /// about every session with tools state; a `false` answer closes with
    /// `SESSION_ENDED` and forgets everything about it.
    pub fn sweep(&self, session_exists: impl Fn(&SessionName) -> bool) {
        let now = Instant::now();
        let expired: Vec<SessionName> = self
            .grants
            .lock()
            .iter()
            .filter(|(_, grant)| grant.expires_at <= now)
            .map(|(name, _)| name.clone())
            .collect();
        for name in expired {
            tracing::info!(session = %name, "tools attach grant expired");
            self.revoke(&name, close_code::TTL_EXPIRED);
        }
        let mut known: Vec<SessionName> = self.grants.lock().keys().cloned().collect();
        known.extend(self.attached.lock().keys().cloned());
        known.extend(self.loopback_keys.lock().keys().cloned());
        known.sort();
        known.dedup();
        for name in known {
            if !session_exists(&name) {
                self.forget_session(&name);
            }
        }
        // Attaches whose socket task has already finished but which, for any
        // reason, are still mapped.
        let stale: Vec<SessionName> = self
            .attached
            .lock()
            .iter()
            .filter(|(_, attached)| attached.is_closed())
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            self.attached.lock().remove(&name);
        }
    }

    /// Close every attach for shutdown.
    pub fn close_all(&self, code: u16) {
        let all: Vec<Arc<Attached>> = self.attached.lock().drain().map(|(_, a)| a).collect();
        for attached in all {
            attached.close(code);
        }
    }

    /// Admin-visible summary, keyed by session.
    pub fn summary(&self) -> Value {
        let mut out = serde_json::Map::new();
        let grants = self.grants.lock();
        let attached = self.attached.lock();
        let mut names: Vec<&SessionName> = grants.keys().chain(attached.keys()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let grant = grants.get(name);
            let live = attached.get(name).filter(|a| !a.is_closed());
            out.insert(
                name.as_str().to_owned(),
                json!({
                    "granted": grant.is_some(),
                    "grant_expires_in_secs": grant
                        .map(|g| g.expires_at.saturating_duration_since(Instant::now()).as_secs()),
                    "attached": live.is_some(),
                    "peer": live.map(|a| a.peer.clone()),
                }),
            );
        }
        Value::Object(out)
    }

    pub fn is_attached(&self, session: &SessionName) -> bool {
        self.attached_for(session).is_some()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn forward(attached: &Attached, id: Value, request: Value) -> Value {
    match attached.call(request).await {
        Ok(mut response) => {
            if let Some(object) = response.as_object_mut() {
                object.insert("id".into(), id);
            }
            response
        }
        Err(error) => rpc_error(id, error.rpc_code(), error.message()),
    }
}

pub fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_error_result(id: Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }], "isError": true }
    })
}

/// Close reason text for the tools socket, covering the codes it can carry.
pub fn close_reason(code: u16) -> &'static str {
    match code {
        close_code::TTL_EXPIRED => "tools grant expired",
        close_code::TAKEOVER => "replaced by a newer attach for this session",
        close_code::SESSION_ENDED => "session ended",
        close_code::RUNTIME_REPLACED => "runtime replaced",
        close_code::TOOLS_REVOKED => "tools grant revoked by the admin plane",
        1000 => "normal closure",
        _ => "closed",
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// 256 random bits as 64 lowercase hex bytes — header-safe, as attach tokens are.
fn random_hex() -> Result<Vec<u8>, Error> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| {
        Error::Other(format!(
            "OS RNG failed while minting a tools attach secret: {e}"
        ))
    })?;
    let encoded = hex::encode(raw).into_bytes();
    for byte in &mut raw {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> SessionName {
        SessionName::parse(name).unwrap()
    }

    #[test]
    fn a_minted_secret_verifies_once_per_presentation_and_is_erased() {
        let hub = ToolsHub::new(Duration::from_secs(60), AuditLogger);
        let s = session("a");
        let minted = hub.mint(&s).unwrap();
        assert_eq!(minted.plaintext.len(), 64);
        let plain = minted.plaintext.as_bytes().to_vec();
        let mut presented = SecretBytes::new(plain.clone());
        hub.verify(&s, &mut presented, "test").unwrap();
        assert!(presented.as_bytes().iter().all(|b| *b == 0));
        // Reusable within the grant (the dialer redials on drop).
        let mut again = SecretBytes::new(plain);
        hub.verify(&s, &mut again, "test").unwrap();
    }

    #[test]
    fn mint_uses_one_hour_by_default_and_honors_a_requested_ttl() {
        let hub = ToolsHub::new(DEFAULT_TOOLS_ATTACH_MAX_TTL, AuditLogger);
        let before = Instant::now();
        let defaulted = hub.mint(&session("defaulted")).unwrap();
        let default_lifetime = defaulted.expires_at.duration_since(before);
        assert!(default_lifetime >= Duration::from_secs(3599));
        assert!(default_lifetime <= Duration::from_secs(3601));

        let before = Instant::now();
        let requested = hub
            .mint_with_ttl(&session("requested"), Duration::from_secs(2 * 60 * 60))
            .unwrap();
        let requested_lifetime = requested.expires_at.duration_since(before);
        assert!(requested_lifetime >= Duration::from_secs(7199));
        assert!(requested_lifetime <= Duration::from_secs(7201));
    }

    #[test]
    fn mint_refuses_zero_and_over_the_operator_maximum_instead_of_capping() {
        let hub = ToolsHub::new(Duration::from_secs(24 * 60 * 60), AuditLogger);
        assert!(hub.mint_with_ttl(&session("zero"), Duration::ZERO).is_err());
        assert!(hub
            .mint_with_ttl(&session("over"), Duration::from_secs(24 * 60 * 60 + 1),)
            .is_err());
    }

    #[test]
    fn a_secret_for_session_a_does_not_open_session_b() {
        let hub = ToolsHub::new(Duration::from_secs(60), AuditLogger);
        let a = session("a");
        let b = session("b");
        let minted = hub.mint(&a).unwrap();

        hub.mint(&b).unwrap();
        let mut presented = SecretBytes::new(minted.plaintext.as_bytes().to_vec());
        assert!(hub.verify(&b, &mut presented, "test").is_err());
    }

    #[test]
    fn revoke_and_expiry_drop_the_grant() {
        let hub = ToolsHub::new(Duration::from_millis(1), AuditLogger);
        let s = session("a");
        let minted = hub.mint(&s).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let mut presented = SecretBytes::new(minted.plaintext.as_bytes().to_vec());
        assert!(hub.verify(&s, &mut presented, "test").is_err());

        let hub = ToolsHub::new(Duration::from_secs(60), AuditLogger);
        let minted = hub.mint(&s).unwrap();
        assert!(hub.revoke(&s, close_code::TOOLS_REVOKED));
        assert!(!hub.revoke(&s, close_code::TOOLS_REVOKED));
        let mut presented = SecretBytes::new(minted.plaintext.as_bytes().to_vec());
        assert!(hub.verify(&s, &mut presented, "test").is_err());
    }

    #[test]
    fn loopback_keys_are_per_session_and_rotate() {
        let hub = ToolsHub::new(Duration::from_secs(60), AuditLogger);
        let a = session("a");
        let b = session("b");
        let ka = hub.issue_loopback_key(&a);
        let kb = hub.issue_loopback_key(&b);
        assert_eq!(ka.len(), 64);
        assert!(hub.check_loopback_key(&a, &ka));
        assert!(!hub.check_loopback_key(&b, &ka));
        assert!(!hub.check_loopback_key(&a, &kb));
        assert!(!hub.check_loopback_key(&a, "zz"));
        let ka2 = hub.issue_loopback_key(&a);
        assert!(!hub.check_loopback_key(&a, &ka));
        assert!(hub.check_loopback_key(&a, &ka2));
    }

    #[tokio::test]
    async fn not_attached_shape_is_status_tool_only_and_tool_errors() {
        let hub = ToolsHub::new(Duration::from_secs(60), AuditLogger);
        let s = session("a");
        let list = hub
            .handle_loopback(&s, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .await
            .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec![STATUS_TOOL]);

        let status = hub
            .handle_loopback(
                &s,
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":STATUS_TOOL,"arguments":{}}}),
            )
            .await
            .unwrap();
        assert_eq!(
            status["result"]["structuredContent"]["attached"],
            json!(false)
        );

        let call = hub
            .handle_loopback(
                &s,
                json!({"jsonrpc":"2.0","id":"x","method":"tools/call","params":{"name":"screenshot","arguments":{}}}),
            )
            .await
            .unwrap();
        assert_eq!(call["id"], json!("x"));
        assert_eq!(call["result"]["isError"], json!(true));

        let init = hub
            .handle_loopback(
                &s,
                json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
            )
            .await
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], json!("2025-03-26"));

        assert!(hub
            .handle_loopback(
                &s,
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .await
            .is_none());

        let other = hub
            .handle_loopback(
                &s,
                json!({"jsonrpc":"2.0","id":4,"method":"resources/list"}),
            )
            .await
            .unwrap();
        assert_eq!(other["error"]["code"], json!(-32601));
    }

    #[test]
    fn sweep_forgets_sessions_that_no_longer_exist() {
        let hub = ToolsHub::new(Duration::from_secs(60), AuditLogger);
        let a = session("a");
        let b = session("b");
        hub.mint(&a).unwrap();
        hub.mint(&b).unwrap();
        hub.issue_loopback_key(&a);
        hub.sweep(|name| name == &a);
        assert!(hub.summary().get("a").is_some());
        assert!(hub.summary().get("b").is_none());
        assert!(hub.check_loopback_key(&a, &hub.issue_loopback_key(&a)));
    }
}
