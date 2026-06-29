---
paths:
  - "**/*.yaml"
  - "**/*.yml"
---

# Kubernetes Workload Rules

Language-agnostic conventions for any emitted Kubernetes manifests (Helm templates, UI,
operator-generated objects, raw YAML). For Rust kube-rs operator patterns, see
[`operator-rust.md`](operator-rust.md).

## Resource Emission Requirements

- Every container must set CPU and memory **requests and limits**.
- Every long-running container must have **readiness and liveness probes**.
- Never emit `:latest` image tags — use explicit tags or digests.
- Set `terminationGracePeriodSeconds`; use PreStop hooks when containers need drain time.
- Deployments must set `strategy.rollingUpdate` with explicit `maxSurge` and `maxUnavailable`.
- Multi-replica Deployments should set `topologySpreadConstraints` for zone awareness.

## Container Security

- Run as a non-root user.
- `allowPrivilegeEscalation: false`.
- `readOnlyRootFilesystem: true`.
- Drop all Linux capabilities (`drop: ["ALL"]`).
- Use distroless or Chainguard base images — avoid ubuntu/debian/alpine (a shell is escalation
  surface). Use `kubectl debug` with ephemeral containers for debugging.

## RBAC

- Declare minimal permissions — only the verbs and resources actually used.
- Prefer namespaced `Role` + `RoleBinding` over cluster-wide `ClusterRole` when the workload
  operates in specific namespaces.
- Scope write access carefully — limit to non-delete verbs and specific `resourceNames` where
  possible.
- **Audit RBAC whenever** code introduces or modifies a Kubernetes API call, a subchart/dependency
  is added or upgraded, or CRD definitions change. Namespace-scoped resources belong in both
  `Role` and `ClusterRole` templates; cluster-scoped resources (namespaces, CRDs, nodes,
  storageclasses) belong only in the `ClusterRole`, and the code using them must degrade gracefully
  in namespace-only installs. New API groups/resources warrant a comment explaining what they're
  for.

## Network Policies

Default-deny ingress and egress; selectively permit:

- DNS resolution (UDP 53 to kube-dns)
- API server communication (TCP 443/6443)
- Observability egress (OTLP ports 4317/4318)
- Prometheus scraping ingress from the monitoring namespace

## Supply Chain

- Pin image digests where reproducibility matters.
- Run dependency/vulnerability scanning (`trivy`, `grype`) on images in CI.
- Embed an SBOM for downstream scanning.
- Automate dependency updates (Dependabot/Renovate) with a review cooldown.
