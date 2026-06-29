---
paths:
  - "**/Dockerfile"
  - "**/Dockerfile.*"
  - "**/*.dockerfile"
  - "**/.dockerignore"
  - "**/docker/**"
---

# Docker Build Rules

## Compile-time file dependencies

When build steps read files at compile time (e.g. a Rust `build.rs`, a generated-code step, an
embedded asset):

1. **Check `.dockerignore`** at the build context root — remove exclusions for any newly-needed
   files, or they won't exist inside the build.
2. **Add `COPY` directives to every stage that runs the build.** With a dependency-caching pattern
   (cargo-chef, multi-stage planner/builder), both the planner *and* the builder stage need the
   file — missing it in one produces confusing cache-vs-build mismatches.
3. **Verify relative paths resolve inside the container.** Paths are relative to the stage's
   `WORKDIR`, not the host repo root.

## Layer caching

- Order `COPY`/`RUN` from least- to most-frequently-changed so dependency layers stay cached across
  source edits (copy manifests + lockfile, fetch deps, *then* copy source).
- When a tool is installed at a pinned version and you want a version bump to bust the cache, prefer
  `RUN curl ... ${VERSION}` with the version as an `ARG` over a multi-stage `COPY` — the `RUN`
  command string changes when the version changes, invalidating exactly that layer.

## Hardening

- Pin base images by digest where reproducibility matters; otherwise pin an explicit tag, never
  `:latest`.
- Run as a non-root `USER`.
- Prefer distroless / Chainguard runtime stages — no shell, smaller attack surface.
- Apply security/CVE pins last so they override transitive dependencies.
