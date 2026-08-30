# llmenv's published default sandbox image (#1653, design doc
# docs/design/issue-1080-sidecar-container.md). Deliberately minimal: just
# enough libc + CA certificates to exec an engine binary that llmenv
# bind-mounts in at container start (src/launch/sandbox.rs) — no engine
# binary is baked in here, so this image doesn't go stale on every engine
# release. `features.sandbox.image` overrides this default with any
# user-supplied image; llmenv performs the same bind-mount-and-exec step
# regardless of whose image it is.
#
# Pinned by content digest, not the mutable `bookworm-slim` tag (#1704): a
# Debian security update or a registry-side swap would otherwise change what
# this image builds from with no corresponding change in this repo.
# Renovate's docker manager (.github/renovate.json5) tracks and automerges
# digest updates for this line — no separate rule needed, the existing
# "digest" update-type automerge rule already covers every manager.
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
