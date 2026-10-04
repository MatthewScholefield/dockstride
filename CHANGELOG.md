# Changelog

## 0.1.0

Dockstride's first release provides one inspectable project convention from editable configuration to Compose development and Swarm deployment.

- Embedded pinned Nickel evaluator/library; independently discoverable lazy configuration/setup metadata, nested contracts/defaults/docs, controlled in-memory YAML candidates, canonical build data, and explicit Compose/Swarm rendering.
- Initialization, incremental interactive/noninteractive setup, atomic locked YAML edits preserving ordinary comments, schema/render/doctor, help/completions, explicit native namespaces, and version-1 JSON events/results/errors.
- Root flow-mapping configuration edits preserve the single YAML document and existing values/comments; secret sessions honor reflected project/backend defaults without copying them into the environment.
- Trusted Compose startup with owned-resource/context checks, persisted declared port allocation, bounded startup logs, successful one-shot prerequisites, native health and HTTP/command application readiness, watch/project development loops, and raw foreground interactive exec.
- Starter/sample development watch explicitly rebuilds the changed service, avoiding Docker rootless archive-copy failures with read-only secret bind mounts; native watch actions are not silently rewritten.
- Private no-follow exclusive secret files, optional explicit root administration setup, non-listable per-user storage, validated non-root/rootless group readability, hidden prompt/file/stdin inputs, and generated secret reuse.
- Default file storage honors `XDG_DATA_HOME`, falling back to `HOME` only when unset/empty; fixtures isolate both roots and retain the original Docker configuration.
- Immutable labelled Swarm secret revisions with cluster scope and pre-publication durable recovery files; bounded native object names retain full identity in labels.
- Immutable image publication/digest pinning, explicit production prerequisites, full no-prune deployments, finite selected-service updates, live-spec reconciliation, rollout diagnostics, and retained deployment intent/snapshots.
- Explicit secret replacement/history, application-specific apply boundaries, protected revision GC, interrupted-operation reconciliation, lifecycle/config/allocation locking, and ownership-scoped teardown preserving secrets by default.
- Meaningful bounded streamed Docker stderr in terminal failure diagnostics; underlying process status is retained. Secret-create diagnostics redact raw/base64 credential bytes without suppressing the backend failure.
- Linux x86_64/ARM64 musl release tooling, deterministic archives/checksums/provenance, independent reproducibility verification, CI release workflow, and disposable real Compose/Swarm fixtures.
- Public GitHub distribution with automatic native builds on main/pull requests and release publication on version tags. A checksum/version-verified curl installer supports latest or pinned releases, user-local/custom destinations, atomic replacement, and real HTTP failure-preservation checks.
- A narrowly documented upstream Nickel fix preserves recursive closure invariants for merged nested deserialized data; vendored provenance and behavioral regressions accompany the patch.

No host/package/Swarm/registry infrastructure changes occur implicitly. No Kubernetes, mandatory daemon, generic workflow engine, universal credential rotation, zero-downtime promise, or transactional migration rollback is claimed.
