# Global ARG — must be declared before the first FROM to be visible inside a
# later FROM. openabdev/openab's Dockerfile.unified has the identical comment for
# the identical reason: an ARG declared between stages is scoped to the stage
# that follows it and is invisible to FROM itself, so putting it there silently
# builds `FROM ghcr.io/openabdev/openab@` with nothing after the @ — this was
# caught by actually running the build, not by reading the Dockerfile spec.
ARG OPENAB_BASE_DIGEST=sha256:f466b5641bb9a687b5ffa8c80ce0e6c470ff91e49586e3a75be6c62a35f57839

# openab-pty — internal image. Not published; see README.
#
# Multi-stage so that `docker build .` from a clean checkout is the whole build.
# The previous shape needed the binary produced out-of-band and dropped into the
# context, which meant the image and the source could disagree with nothing to
# detect it.
#
# Both stages are pinned by digest. The builder digest is the same one
# openabdev/openab pins in Dockerfile.unified, and for the same stated reason: a
# rolling tag lets a base change alter the output of an unchanged commit, and in
# CI nobody is watching when that happens.

# --- build ------------------------------------------------------------------
FROM docker.io/library/rust:1-bookworm@sha256:e70e2eec3d495fd5c8e0be74adda86507dfac7f51a724fbf9813ff59b2b247c7 AS builder

# musl, so the result is immune to whatever libc the runtime base ships. This is
# not theoretical tidiness: the runtime base is a separately released image, and
# a static binary is the reason a base bump cannot break the process.
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

WORKDIR /src
COPY runtime/Cargo.toml runtime/Cargo.lock ./
# Prime the dependency layer against the manifests alone, so editing src/ does
# not re-download and rebuild the tree.
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release --locked --target x86_64-unknown-linux-musl \
    && rm -rf src

COPY runtime/src ./src
# The touch is load-bearing, not tidiness. Cargo decides freshness by mtime, COPY
# preserves the context's timestamps, and if those land older than the stub
# artifacts above then cargo declares the stub build fresh and the image ships a
# binary whose main() does nothing -- a failure that builds green and only shows
# up as a container that exits instantly.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked --target x86_64-unknown-linux-musl \
    && strip target/x86_64-unknown-linux-musl/release/openab-pty

# --- runtime ----------------------------------------------------------------
# The OAB image is the point rather than a convenience: it already carries the
# agent CLI and already runs as uid 1000, so a session opened here lands in the
# same toolchain the agent uses. That adjacency is the ADR's stated positioning,
# and it is why this image is ~700 MB of base plus a few MB of runtime rather
# than a 20 MB scratch image that would be useless for the purpose.
#
# Pinned to the amd64 manifest inside ghcr.io/openabdev/openab:pre-beta-kiro, not
# :latest. Checked against the fleet rather than assumed: B0 (the real running
# bot) is deployed on pre-beta-kiro, and at the time this was pinned that tag was
# openab 0.10.0 while :latest was still 0.9.0 -- two different images, not two
# names for one. Adjacency means matching what an agent actually runs next to,
# so the pin follows the fleet's tag rather than the one that sounds newest.
#
# A digest, not the tag, for the same reason AGENTS.md pins the Rust builder in
# Dockerfile.unified: a rolling tag lets a base change alter an unchanged
# commit's output, with nothing in the build log to say so. Staying current with
# pre-beta-kiro is still wanted, so it happens on a schedule that opens a PR
# instead of a build that silently re-resolves — see
# .github/workflows/bump-base-image.yml. Update OPENAB_BASE_DIGEST's default
# above by hand with:
#   T=$(curl -s "https://ghcr.io/token?scope=repository:openabdev/openab:pull&service=ghcr.io" | jq -r .token)
#   curl -s -H "Authorization: Bearer $T" \
#     -H 'Accept: application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json' \
#     https://ghcr.io/v2/openabdev/openab/manifests/pre-beta-kiro | jq -r '.manifests[]|select(.platform.architecture=="amd64").digest'
# LICENSING BOUNDARY. This repository is MIT (see LICENSE). That covers the recipe
# below, not what the recipe pulls in: the image built from this line aggregates
# the openab base and, for every variant except `native`, a third-party agent CLI
# under its vendor's own terms. MIT does not and cannot relicense those. See NOTICE.
FROM ghcr.io/openabdev/openab@${OPENAB_BASE_DIGEST}

COPY --from=builder --chown=root:root --chmod=755 \
    /src/target/x86_64-unknown-linux-musl/release/openab-pty /usr/local/bin/openab-pty
# --chmod is deliberate, not decoration: entrypoint.sh is 644 in the repository
# (nobody had ever set +x on it — the prior single-stage build only worked
# because it started from a binary built and chmod'd elsewhere) and COPY
# preserves the source mode by default. Without this the image builds clean and
# fails at `crun: open executable: Permission denied` on every single container,
# including as root, which is a confusing way to learn a file has no x bit.
COPY --chown=root:root --chmod=755 deploy/ecs/entrypoint.sh /usr/local/bin/openab-pty-entrypoint

USER 1000
# The listener default is loopback, and the runtime refuses an off-loopback bind
# while it holds no TLS key. Reachability is the deployment's job — a tailscale
# sidecar sharing the network namespace — not this image's.
ENV HOME=/workspace \
    RUST_LOG=info \
    PTY_LISTEN=127.0.0.1:8090 \
    PTY_COMMAND=/usr/bin/bash \
    PTY_MAX_SESSIONS=3 \
    PTY_ABSOLUTE_TTL=12h \
    PTY_IDLE_TTL=8h \
    PTY_TOKEN_TTL=12h \
    PTY_SCROLLBACK_KIB=1024 \
    PTY_FILTER_TERMINAL_RESPONSES=true \
    # Empty means no seeding, which is what every deployment did before this
    # existed. A deployment opts in by pointing this at a directory another
    # container has filled with *.tar.gz; this image fetches nothing itself.
    PTY_SEED_DIR=""

ENTRYPOINT ["/usr/local/bin/openab-pty-entrypoint"]
