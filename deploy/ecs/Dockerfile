# openab-pty:pre-beta-kiro
#
# The OAB image already carries kiro-cli and runs as uid 1000, so this adds
# exactly two things: the statically linked runtime, and an entrypoint that
# materialises the config projection from the environment.
#
# Why an entrypoint writes the config instead of baking it in: the delivered
# projection must carry a literal `admin_credential_hash` — the validator rejects
# `${VAR}` on that key specifically, because a fail-open there would let a
# poisoned projection disable admin auth. So the hash arrives through the
# environment (it is a non-reversible verifier, not a credential) and the
# entrypoint writes a projection containing the literal value.
#
# Deliberately NOT in this image: tailscale. It stays a sidecar using the
# official image, which mirrors the Kubernetes shape and avoids owning
# tailscale's release cadence.
FROM ghcr.io/openabdev/openab:latest

# The runtime is a fully static musl binary, so it runs unchanged on this
# glibc-based image.
COPY --chown=root:root openab-pty /usr/local/bin/openab-pty
COPY --chown=root:root entrypoint.sh /usr/local/bin/openab-pty-entrypoint

USER 1000
ENV HOME=/workspace \
    RUST_LOG=info \
    PTY_LISTEN=127.0.0.1:8090 \
    PTY_COMMAND=/usr/bin/bash \
    PTY_MAX_SESSIONS=3 \
    PTY_ABSOLUTE_TTL=12h \
    PTY_IDLE_TTL=8h \
    PTY_TOKEN_TTL=12h \
    PTY_SCROLLBACK_KIB=1024

ENTRYPOINT ["/usr/local/bin/openab-pty-entrypoint"]
