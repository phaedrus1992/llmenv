# llmenv's published default sandbox image (#1653, design doc
# docs/design/issue-1080-sidecar-container.md). Deliberately minimal: just
# enough libc + CA certificates to exec an engine binary that llmenv
# bind-mounts in at container start (src/launch/sandbox.rs) — no engine
# binary is baked in here, so this image doesn't go stale on every engine
# release. `features.sandbox.image` overrides this default with any
# user-supplied image; llmenv performs the same bind-mount-and-exec step
# regardless of whose image it is.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
