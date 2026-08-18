#!/usr/bin/env bash
# Materialise the [pty] projection, then hand off to the runtime.
#
# The hash is a non-reversible verifier, so carrying it in the environment is
# acceptable; the credential itself never appears here, in argv, or on disk.
set -euo pipefail

if [[ -z "${PTY_ADMIN_HASH:-}" ]]; then
    echo "openab-pty-entrypoint: PTY_ADMIN_HASH is required (sha256:<64 hex>)" >&2
    echo "  generate one with: openab-pty --generate-admin-credential" >&2
    exit 64
fi

CONFIG_DIR="${PTY_CONFIG_DIR:-/tmp/openab-pty}"
mkdir -p "$CONFIG_DIR"
CONFIG="$CONFIG_DIR/config.toml"

# tls_terminated_upstream is false and honest: the listener is loopback-only, so
# nothing is served off-loopback at all. Reachability comes from the tailscale
# sidecar sharing this task's network namespace and proxying inbound connections
# to localhost, and the encrypted hop is WireGuard rather than a claimed
# upstream TLS terminator.
umask 077
cat > "$CONFIG" <<EOF
[pty]
enabled = true
listen = "${PTY_LISTEN}"
tls_terminated_upstream = false
command = "${PTY_COMMAND}"
max_sessions = ${PTY_MAX_SESSIONS}
absolute_session_ttl = "${PTY_ABSOLUTE_TTL}"
detached_idle_ttl = "${PTY_IDLE_TTL}"
attach_token_ttl = "${PTY_TOKEN_TTL}"
scrollback_kib = ${PTY_SCROLLBACK_KIB}
scrollback_replay = false
admin_credential_hash = "${PTY_ADMIN_HASH}"
seed_dir = "${PTY_SEED_DIR:-}"
EOF

# Fail before serving rather than after: the same validator the runtime applies
# at startup, so a bad projection is a clear message instead of a crash loop.
/usr/local/bin/openab-pty --validate-projection "$CONFIG"

exec /usr/local/bin/openab-pty --config "$CONFIG" "$@"
