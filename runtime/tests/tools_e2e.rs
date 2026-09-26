//! End-to-end tests for the reverse-attached tools plane: a fake Mac dials
//! `WS /tools/attach/{session}`, and a caller on the loopback MCP endpoint —
//! using the URL the session's *shell* was actually given — reaches it.
//!
//! `#[ignore]`d like `e2e.rs`, for the same reason (sockets and PTY children).
//! Run with `cargo test -p openab-pty -- --ignored --test-threads=1`.

use futures_util::{SinkExt, StreamExt};
use openab_pty::admin_auth::AdminAuthenticator;
use openab_pty::audit::AuditLogger;
use openab_pty::close_code;
use openab_pty::config;
use openab_pty::killdomain::{KillDomain, TrackingLimits};
use openab_pty::server::{self, AppState, ServerConfig};
use openab_pty::session::{PortablePtySpawner, SessionManager, SessionPolicy, TOOLS_URL_ENV};
use openab_pty::token::TokenStore;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite;

const IO_TIMEOUT: Duration = Duration::from_secs(30);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    addr: SocketAddr,
    tools_addr: SocketAddr,
    admin_credential: String,
    manager: Arc<SessionManager>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    served: Option<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn start(tools_ttl: Duration) -> Self {
        let audit = AuditLogger;
        let generated = AdminAuthenticator::generate();
        let admin_credential =
            String::from_utf8(generated.plaintext.as_bytes().to_vec()).expect("hex credential");
        let projection = format!(
            r#"[pty]
enabled = true
listen = "127.0.0.1:0"
tls_terminated_upstream = true
command = "/bin/sh"
max_sessions = 4
absolute_session_ttl = "12h"
scrollback_kib = 1024
scrollback_replay = false
admin_credential_hash = "{}"
tools_listen = "127.0.0.1:0"
tools_attach_ttl = "1h"
"#,
            generated.verifier
        );
        let parsed = config::validate_projection(&projection).expect("projection validates");

        let spawner = Arc::new(PortablePtySpawner);
        let kill = Arc::new(KillDomain::new(TrackingLimits::default(), audit.clone()));
        let tokens = TokenStore::new(Duration::from_secs(600), audit.clone());
        let verifier = tokens.attach_verifier();
        let admin = AdminAuthenticator::with_limits(
            &generated.verifier,
            4,
            Duration::from_millis(1),
            audit.clone(),
        )
        .expect("admin authenticator");
        let policy = SessionPolicy::from_config(&parsed);
        let manager =
            SessionManager::new(parsed.clone(), policy, tokens, audit.clone(), kill, spawner)
                .expect("session manager");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tools_listener = server::bind_tools(&parsed.tools_listen).await.unwrap();
        let tools_addr = tools_listener.local_addr().unwrap();
        let state = AppState::new(
            manager.clone(),
            verifier,
            admin,
            audit,
            ServerConfig {
                listen: addr.to_string(),
                tls_terminated_upstream: true,
                drain_grace: Duration::ZERO,
                // Fast ticks so grant expiry is observable inside a test.
                tick_interval: Duration::from_millis(50),
                tools_attach_ttl: tools_ttl,
            },
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let served = tokio::spawn(async move {
            let _ = server::serve_with_tools(state, listener, Some(tools_listener), async move {
                let _ = stopped.await;
            })
            .await;
        });
        Self {
            addr,
            tools_addr,
            admin_credential,
            manager,
            stop: Some(stop),
            served: Some(served),
        }
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(served) = self.served.take() {
            let _ = tokio::time::timeout(IO_TIMEOUT, served).await;
        }
        let _ = self.manager.shutdown().await;
    }

    async fn admin(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let (status, raw) =
            http_request(self.addr, method, path, Some(&self.admin_credential), body).await;
        (status, serde_json::from_str(&raw).unwrap_or(Value::Null))
    }

    async fn create(&self, name: &str) -> String {
        let (status, body) = self
            .admin(
                "POST",
                "/admin/sessions",
                Some(&format!(r#"{{"name":"{name}","rows":24,"cols":80}}"#)),
            )
            .await;
        assert_eq!(status, 200, "create failed: {body}");
        body["token"].as_str().unwrap().to_owned()
    }

    async fn mint_tools(&self, name: &str) -> String {
        let (status, body) = self
            .admin(
                "POST",
                &format!("/admin/sessions/{name}/tools-attach"),
                None,
            )
            .await;
        assert_eq!(status, 201, "tools mint failed: {body}");
        assert!(body["verifier"].as_str().unwrap().starts_with("sha256:"));
        body["secret"].as_str().unwrap().to_owned()
    }

    /// Attach to the PTY, ask the shell for its tools URL, and return it. This
    /// is what makes the tests below honest: the URL under test is the one the
    /// child process was actually handed, not one the test computed.
    async fn shell_tools_url(&self, name: &str, token: &str) -> String {
        let mut socket = ws_connect(
            &format!("ws://{}/pty/{name}", self.addr),
            ("authorization", format!("Bearer {token}")),
        )
        .await
        .expect("pty attach");
        // Consume the attach notice.
        let _ = next_text(&mut socket).await;
        socket
            .send(tungstenite::Message::Binary(
                format!("echo TOOLS=${TOOLS_URL_ENV}=END\n")
                    .into_bytes()
                    .into(),
            ))
            .await
            .unwrap();
        let mut collected = Vec::new();
        let url = loop {
            let message = tokio::time::timeout(IO_TIMEOUT, socket.next())
                .await
                .expect("timed out waiting for shell output")
                .expect("socket closed")
                .expect("ws error");
            if let tungstenite::Message::Binary(bytes) = message {
                collected.extend_from_slice(&bytes);
                let text = String::from_utf8_lossy(&collected).to_string();
                // Skip the echoed command line: take the last match, which is
                // the expansion.
                if let Some(start) = text.rfind("TOOLS=http") {
                    if let Some(end) = text[start..].find("=END") {
                        break text[start + "TOOLS=".len()..start + end].to_string();
                    }
                }
            }
        };
        let _ = socket.close(None).await;
        url
    }
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    credential: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(credential) = credential {
        request.push_str(&format!("Authorization: Bearer {credential}\r\n"));
    }
    match body {
        Some(body) => {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            request.push_str(body);
        }
        None => request.push_str("\r\n"),
    }
    let mut stream = tokio::time::timeout(IO_TIMEOUT, tokio::net::TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(IO_TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .expect("read timeout")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let (headers, body) = text.split_once("\r\n\r\n").unwrap_or(("", ""));
    let body = if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)
    } else {
        body.to_string()
    };
    (status, body)
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&after[..size.min(after.len())]);
        rest = after.get(size + 2..).unwrap_or("");
    }
    out
}

/// POST one JSON-RPC message to a full loopback URL (`http://host:port/path`).
async fn mcp_post(url: &str, message: Value) -> (u16, Value) {
    let without_scheme = url.strip_prefix("http://").expect("http url");
    let (host, path) = without_scheme.split_once('/').expect("path");
    let addr: SocketAddr = host.parse().expect("socket addr in url");
    let (status, raw) = http_request(
        addr,
        "POST",
        &format!("/{path}"),
        None,
        Some(&message.to_string()),
    )
    .await;
    (status, serde_json::from_str(&raw).unwrap_or(Value::Null))
}

async fn ws_connect(url: &str, header: (&str, String)) -> Result<Socket, u16> {
    use tungstenite::client::IntoClientRequest;
    let mut request = url.to_string().into_client_request().unwrap();
    request.headers_mut().insert(
        tungstenite::http::HeaderName::from_bytes(header.0.as_bytes()).unwrap(),
        tungstenite::http::HeaderValue::from_str(&header.1).unwrap(),
    );
    match tokio::time::timeout(IO_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Err(_) => panic!("websocket handshake timed out"),
        Ok(Ok((socket, _))) => Ok(socket),
        Ok(Err(tungstenite::Error::Http(response))) => Err(response.status().as_u16()),
        Ok(Err(error)) => panic!("unexpected websocket error: {error}"),
    }
}

async fn next_text(socket: &mut Socket) -> Option<Value> {
    loop {
        let message = tokio::time::timeout(IO_TIMEOUT, socket.next())
            .await
            .expect("timed out waiting for a frame")?
            .ok()?;
        match message {
            tungstenite::Message::Text(text) => return serde_json::from_str(&text).ok(),
            tungstenite::Message::Close(_) => return None,
            _ => continue,
        }
    }
}

/// A stand-in for `oab-instance-mcp`'s reverse-attach client: dials in, then
/// behaves as a tiny MCP server with two tools until the socket closes. Returns
/// the close code it observed.
async fn fake_mac(
    harness_addr: SocketAddr,
    session: &str,
    secret: &str,
    label: &str,
) -> Result<tokio::task::JoinHandle<u16>, u16> {
    let mut socket = ws_connect(
        &format!("ws://{harness_addr}/tools/attach/{session}"),
        ("authorization", format!("Bearer {secret}")),
    )
    .await?;
    let label = label.to_owned();
    Ok(tokio::spawn(async move {
        loop {
            let message = match tokio::time::timeout(IO_TIMEOUT, socket.next()).await {
                Ok(Some(Ok(message))) => message,
                Ok(Some(Err(_))) | Ok(None) => return 1006,
                Err(_) => return 0,
            };
            let text = match message {
                tungstenite::Message::Text(text) => text.to_string(),
                tungstenite::Message::Close(Some(frame)) => return frame.code.into(),
                tungstenite::Message::Close(None) => return 1005,
                tungstenite::Message::Ping(payload) => {
                    let _ = socket.send(tungstenite::Message::Pong(payload)).await;
                    continue;
                }
                _ => continue,
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            let Some(id) = request.get("id").cloned() else {
                continue;
            };
            let method = request["method"].as_str().unwrap_or_default();
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake-mac", "version": label }
                }),
                "tools/list" => json!({ "tools": [
                    { "name": "sys_info", "description": "who am I", "inputSchema": { "type": "object" } },
                    { "name": "echo", "description": "echo", "inputSchema": { "type": "object" } }
                ]}),
                "tools/call" => {
                    let name = request["params"]["name"].as_str().unwrap_or_default();
                    let text = match name {
                        "sys_info" => format!("fake mac {label}"),
                        "echo" => request["params"]["arguments"]["text"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        _ => format!("unknown tool {name}"),
                    };
                    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
                }
                _ => {
                    let reply = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "nope" } });
                    let _ = socket
                        .send(tungstenite::Message::Text(reply.to_string().into()))
                        .await;
                    continue;
                }
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            if socket
                .send(tungstenite::Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                return 1006;
            }
        }
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "binds sockets and spawns a PTY child"]
async fn the_shell_reaches_a_dialled_in_mac_through_its_loopback_url() {
    let h = Harness::start(Duration::from_secs(60)).await;
    let token = h.create("alpha").await;
    let url = h.shell_tools_url("alpha", &token).await;
    assert!(
        url.starts_with(&format!("http://{}/mcp/alpha/", h.tools_addr)),
        "shell env carries the loopback URL: {url}"
    );

    // Not attached yet: status tool only, and a tool call is a tool error, not a
    // protocol failure.
    let (status, list) =
        mcp_post(&url, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
    assert_eq!(status, 200);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["instance_status"]);
    let (_, call) = mcp_post(&url, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"sys_info","arguments":{}}})).await;
    assert_eq!(call["result"]["isError"], json!(true));

    // The Mac dials in.
    let secret = h.mint_tools("alpha").await;
    let mac = fake_mac(h.addr, "alpha", &secret, "one")
        .await
        .expect("attach accepted");

    // initialize is answered locally and reports the attach.
    let (_, init) = mcp_post(&url, json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}})).await;
    assert_eq!(
        init["result"]["serverInfo"]["name"],
        json!("openab-pty-tools")
    );
    let (status, _) = mcp_post(
        &url,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert_eq!(status, 202);

    // Wait for the hub's own initialize toward the Mac to land, then list.
    let mut merged = Vec::new();
    for _ in 0..50 {
        let (_, list) = mcp_post(&url, json!({"jsonrpc":"2.0","id":4,"method":"tools/list"})).await;
        merged = list["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        if merged.len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        merged,
        vec!["sys_info", "echo", "instance_status"],
        "Mac tools plus the local status tool"
    );

    // A forwarded call keeps the caller's id (a string here) end to end.
    let (_, call) = mcp_post(&url, json!({"jsonrpc":"2.0","id":"req-a","method":"tools/call","params":{"name":"echo","arguments":{"text":"round trip"}}})).await;
    assert_eq!(call["id"], json!("req-a"));
    assert_eq!(call["result"]["content"][0]["text"], json!("round trip"));

    let (_, status_call) = mcp_post(&url, json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"instance_status","arguments":{}}})).await;
    assert_eq!(
        status_call["result"]["structuredContent"]["attached"],
        json!(true)
    );
    assert_eq!(
        status_call["result"]["structuredContent"]["server"]["name"],
        json!("fake-mac")
    );

    // Admin sees it.
    let (_, listing) = h.admin("GET", "/admin/sessions", None).await;
    assert_eq!(listing["tools"]["alpha"]["attached"], json!(true));

    // Revoke: the Mac is told why, and the shell is back to not-attached.
    let (status, _) = h
        .admin("DELETE", "/admin/sessions/alpha/tools-attach", None)
        .await;
    assert_eq!(status, 204);
    assert_eq!(mac.await.unwrap(), close_code::TOOLS_REVOKED);
    let (_, list) = mcp_post(&url, json!({"jsonrpc":"2.0","id":6,"method":"tools/list"})).await;
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 1);
    // And the old secret is dead.
    assert_eq!(
        fake_mac(h.addr, "alpha", &secret, "again").await.err(),
        Some(401)
    );

    h.shutdown().await;
}

#[tokio::test]
#[ignore = "binds sockets and spawns a PTY child"]
async fn a_second_mac_replaces_the_first_with_takeover() {
    let h = Harness::start(Duration::from_secs(60)).await;
    let token = h.create("beta").await;
    let url = h.shell_tools_url("beta", &token).await;
    let secret = h.mint_tools("beta").await;
    let first = fake_mac(h.addr, "beta", &secret, "first").await.unwrap();
    // Same secret, redialled: the runtime converges on the newest dialer instead
    // of letting two fight (the 4a spike's Address-in-use storm).
    let second = fake_mac(h.addr, "beta", &secret, "second").await.unwrap();
    assert_eq!(first.await.unwrap(), close_code::TAKEOVER);
    let mut who = String::new();
    for _ in 0..50 {
        let (_, call) = mcp_post(&url, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"sys_info","arguments":{}}})).await;
        who = call["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if who.contains("second") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(who, "fake mac second");
    h.admin("DELETE", "/admin/sessions/beta/tools-attach", None)
        .await;
    let _ = second.await;
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "binds sockets and spawns a PTY child"]
async fn grant_expiry_closes_the_mac_with_ttl_expired() {
    let h = Harness::start(Duration::from_millis(400)).await;
    h.create("gamma").await;
    let secret = h.mint_tools("gamma").await;
    let mac = fake_mac(h.addr, "gamma", &secret, "short").await.unwrap();
    assert_eq!(mac.await.unwrap(), close_code::TTL_EXPIRED);
    assert_eq!(
        fake_mac(h.addr, "gamma", &secret, "late").await.err(),
        Some(401)
    );
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "binds sockets and spawns a PTY child"]
async fn killing_the_session_ends_the_attach() {
    let h = Harness::start(Duration::from_secs(60)).await;
    h.create("delta").await;
    let secret = h.mint_tools("delta").await;
    let mac = fake_mac(h.addr, "delta", &secret, "x").await.unwrap();
    let (status, _) = h.admin("DELETE", "/admin/sessions/delta", None).await;
    assert_eq!(status, 200);
    assert_eq!(mac.await.unwrap(), close_code::SESSION_ENDED);
    h.shutdown().await;
}

/// Adversary: a shell in session A holds A's loopback URL and can watch A's
/// traffic. It must not be able to mint for B, attach as B's Mac, or call B's
/// tools — and it must not learn whether B exists by probing.
#[tokio::test]
#[ignore = "binds sockets and spawns PTY children"]
async fn a_session_cannot_reach_its_neighbours_tools() {
    let h = Harness::start(Duration::from_secs(60)).await;
    let token_a = h.create("aaa").await;
    let token_b = h.create("bbb").await;
    let url_a = h.shell_tools_url("aaa", &token_a).await;
    let url_b = h.shell_tools_url("bbb", &token_b).await;
    let secret_b = h.mint_tools("bbb").await;
    let mac_b = fake_mac(h.addr, "bbb", &secret_b, "b").await.unwrap();

    // 1. No admin credential, no mint: same 401 as every admin op.
    let (status, _) = http_request(
        h.addr,
        "POST",
        "/admin/sessions/bbb/tools-attach",
        None,
        None,
    )
    .await;
    assert_eq!(status, 401);
    let (status, _) = http_request(
        h.addr,
        "POST",
        "/admin/sessions/bbb/tools-attach",
        Some(&token_a),
        None,
    )
    .await;
    // 401, or 429 once the per-source admin throttle has engaged from the
    // attempt above: either way the mint did not happen.
    assert!(
        status == 401 || status == 429,
        "a PTY attach token is not an admin credential (got {status})"
    );
    let (_, listing) = h.admin("GET", "/admin/sessions", None).await;
    assert_eq!(
        listing["tools"]["aaa"],
        Value::Null,
        "nothing was minted for A by the adversary"
    );

    // 2. A's PTY token does not open B's tools socket, nor A's own.
    assert_eq!(
        ws_connect(
            &format!("ws://{}/tools/attach/bbb", h.addr),
            ("authorization", format!("Bearer {token_a}"))
        )
        .await
        .err(),
        Some(401)
    );
    assert_eq!(
        ws_connect(
            &format!("ws://{}/tools/attach/aaa", h.addr),
            ("authorization", format!("Bearer {token_a}"))
        )
        .await
        .err(),
        Some(401)
    );

    // 3. A's loopback key on B's path, B's key shape guessed, and a nonexistent
    //    session all look identical: 404.
    let key_a = url_a.rsplit('/').next().unwrap().to_owned();
    let base = format!("http://{}", h.tools_addr);
    let probe = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    let (s1, _) = mcp_post(&format!("{base}/mcp/bbb/{key_a}"), probe.clone()).await;
    let (s2, _) = mcp_post(&format!("{base}/mcp/bbb/{}", "0".repeat(64)), probe.clone()).await;
    let (s3, _) = mcp_post(&format!("{base}/mcp/nonexistent/{key_a}"), probe.clone()).await;
    assert_eq!((s1, s2, s3), (404, 404, 404));

    // 4. Meanwhile B, with its own URL, does reach its Mac; A does not.
    let mut b_names = 0usize;
    for _ in 0..50 {
        let (_, list) = mcp_post(&url_b, probe.clone()).await;
        b_names = list["result"]["tools"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        if b_names == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(b_names, 3);
    let (_, a_list) = mcp_post(&url_a, probe).await;
    assert_eq!(
        a_list["result"]["tools"].as_array().unwrap().len(),
        1,
        "A sees only instance_status"
    );

    h.admin("DELETE", "/admin/sessions/bbb/tools-attach", None)
        .await;
    let _ = mac_b.await;
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "binds sockets"]
async fn the_tools_listener_refuses_a_non_loopback_bind() {
    let error = server::bind_tools("0.0.0.0:0")
        .await
        .expect_err("must refuse");
    assert!(error.to_string().contains("loopback"), "{error}");
    let bad = config::validate_projection(&format!(
        r#"[pty]
enabled = true
listen = "127.0.0.1:0"
command = "/bin/sh"
admin_credential_hash = "{}"
tools_listen = "0.0.0.0:9000"
"#,
        AdminAuthenticator::generate().verifier
    ));
    assert!(
        bad.is_err(),
        "config must refuse a non-loopback tools_listen"
    );
}
