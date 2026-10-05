# Changelog

## Unreleased

- Named, argv-only project commands with lazy bootstrap discovery, pinned Docker context, bounded JSON capture, and manual `dks run NAME` execution.
- Live recursive ordinary-settings sources with provenance, validated local/shared edits, and once-per-invocation missing-value defaults discovery. Explicit empty source selections disable discovery.
- One cross-cutting mutation lock hierarchy; fingerprint-bound snapshots and unlocked editor execution prevent deadlocks and lost concurrent local/shared updates.
- Recoverable ordinary-file publication for shared-source creation, settings, and allocations; pending intents retain conservative blockers and external edits are preserved as recoverable conflicts.
- Saved environment registry, Git worktree discovery, stopped-checkout daemon/project collision enforcement, and explicit resource-safe registration forgetting.
- Versioned generated/explicit endpoint provenance, invoking-only legacy reconciliation, journaled `ports release`, and conservative recorded-target reservation GC. Required native port fields can be filled during initial setup and after reset without transient placeholder configuration.
- Native Compose stop/fresh-prerequisite actions with validated dependency scope, exact completion-ID reuse, dependency-ordered `--no-deps` startup, safe consumer restoration, and one lifecycle deadline. Failure/cancellation retains data and logs without resuming stopped consumers or seeding; secret-rotation actions preflight before reference publication.
- Strict bounded Compose/Swarm status with complete success/failure reports, once-per-service application checks, required dependency/profile scope, saved applied profiles, replica/rollout and successful-task verification, and explicit inspection-only mode.
- Swarm task containers receive ownership labels; existing unlabeled tasks require exact container/task/service ID linkage to an owned service. Owned sparse Compose rendering no longer creates invalid null resource sections.
- Validated read-only diagnostic hooks and explicit doctor execution share one bounded phase, forward only verified resource IDs and native secret references, preserve original startup failures, and return inert findings/suggestions or secondary hook failures.
- Explicit imported-secret synchronization records private canonical file provenance and keyed digests, skips proven unchanged inputs, handles legacy Compose/Swarm baselines honestly, and preserves committed storage after application failure. GC protects registered cross-checkout source paths, aliases, and retained snapshots with fail-closed evidence revalidation.
- Pin the embedded/sample/reference-consumer library snapshot to 0.2.0. Voxellum uses data-only primary-worktree defaults, native shared preferences/provider inputs, native fresh migrations, read-only service-network PostgreSQL diagnostics, and explicit real-registry E2E retirement.
- Disappearing Swarm tasks invalidate the entire observation rather than aborting a healthy rollout or claiming stale/zero-task readiness; convergence retains its existing deadline and other Docker failures remain fatal.
- Owned Swarm network teardown handles exact immutable-ID disappearance without hiding backend failures; known absent removal stops repeated deletion requests and waits for actual inspected absence under the original deadline. Ownership and named-volume recreation checks remain enforced.
- Expand real consumer smoke coverage and ownership-verified DIND failure retirement. Private bind storage honors `TMPDIR`; unknown nodes or failed native teardown retain recovery state instead of hiding claims or pruning host Docker storage.

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
