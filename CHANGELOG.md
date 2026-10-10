# Changelog

## 0.2.1

- Validate deploy service fields against the adapted Swarm manifest instead of rejecting legitimate Compose-only `restart`/`container_name` settings retained in the canonical model. Preserve canonical build validation before publication.
- Accept native legacy stack `expose` alongside `init`; retain these fields and explicit `deploy.restart_policy` in Swarm rendering. Add regression coverage for CLI rendering/read-only deployment plans, adapter parity, and unsupported-field/build preflight safety.

## 0.2.0

- Cut over to YAML/source files, referenced credential bytes and live Docker as durable authorities. `.dockstride` holds disposable locks/editor/render scratch only; remove registry, ownership UUIDs, saved targets, reservations, journals, deployment snapshots and secret histories/HMAC/GC.
- Own managed resources by canonical UTF-8 checkout path plus effective project labels. Preserve sanitized folder proposals and explicit/inherited project names; reject foreign/missing/legacy UUID labels without adoption. Capture Docker connection per invocation, not across invocations.
- Publish comment-preserving single-document YAML edits atomically with sorted source guards and stale-source verification. Editors/defaults/trusted actions run outside config/source guards; create-if-missing shared sources cannot overwrite a concurrent document.
- Allocate missing/null Compose policy ports once as ordinary YAML values, independent of schema-only defaults. Preserve explicit/inherited values, held batch probes, overlap exclusion and exhausted-range unchanged YAML. Remove ports release/GC.
- Make `env list` Git-only invoking-repository worktree inventory; remove registry listing, `--worktrees` and env forget.
- Keep credential files outside repositories under XDG/HOME per-user storage or an explicit secure parent. New private directories are `0700`, existing compatible private directories remain unchanged, and markers/recovery copies are neither required nor erased. Every file input references its original securely without copying/chmod/chown.
- Keep generated/prompt/stdin/reference policies; reject removed FileSecret/durable/recovery contracts. Publish generated file refs before Docker so retries never regenerate missing or previously committed bytes.
- Store current Swarm object names only in local `_dockstride.swarmSecrets`; retain live shared source refs unchanged. Ordinary setup reuses objects without content refresh; explicit sync always publishes a fresh immutable Swarm object or validates the original Compose file. Failed declared rotation preserves committed bindings and reports exact in-memory partial progress; old material is operator-managed.
- Preserve native Compose dependency/profile selection, fresh prerequisite generation revalidation, bounded readiness/cancellation, safe stopped-consumer failure and data retention. Profiles come only from current flags/environment.
- Keep full-stack Swarm authored-tag build/push/digest pinning, distribution guards, explicit deployment commands, native update/rollback policies, live convergence/status and ownership-reverified teardown. Remove positional selected deployment and all historical baseline APIs; no prune.
- Preserve strict schema-v1 status/errors and read-only diagnostics with verified container IDs, exact Swarm service/task linkage and safe secret references, never credential bytes/environment/health logs.
- Pin embedded/sample/Voxellum library API to `0.3.0`. Voxellum retains live shared preferences/direct provider refs, ports, fresh migrations, diagnostic/default hooks and backend declarations; generated credentials use external storage. E2E uses sibling stack/data and verified run-owned retirement, retaining failure evidence.
- Migrate behavioral fixtures and existing real Compose/DIND smoke to current authorities, including foreign-path collisions, scratch removal, explicit every-sync publication, full updates/native rollback and independently verified cleanup. No compatibility readers, automatic legacy migration or erasure of old credentials/resources.

## 0.1.0

Dockstride's first release provides one inspectable project convention from editable configuration to Compose development and Swarm deployment.

- Embedded pinned Nickel evaluator/library; independently discoverable lazy configuration/setup metadata, nested contracts/defaults/docs, controlled in-memory YAML candidates, canonical build data, and explicit Compose/Swarm rendering.
- Initialization, incremental interactive/noninteractive setup, atomic locked YAML edits preserving ordinary comments, schema/render/doctor, help/completions, explicit native namespaces, and version-1 JSON events/results/errors.
- Root flow-mapping configuration edits preserve the single YAML document and existing values/comments; secret sessions honor reflected project/backend defaults without copying them into the environment.
- Compose startup with owned-resource/context checks, persisted declared port allocation, bounded startup logs, successful one-shot prerequisites, native health and HTTP/command application readiness, watch/project development loops, and raw foreground interactive exec.
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
