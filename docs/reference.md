# Dockstride reference

## Configuration boundary

A project has one checked-in Nickel architecture and one editable environment mapping. `dks init` creates a commented `compose.ncl` with a prebuilt hello-world service, the pinned and inspectable `libs/dockstride.ncl` (not a submodule), and an empty `env.yaml` mapping if absent. It preserves existing environment values and `.gitignore` contents, adding only missing `env.yaml` and `.dockstride/` ignore rules. It refuses to overwrite the definition or library, creates no Dockerfile or application source, and performs no evaluation-time fetch. The starter requires only `project`; `backend` defaults to `compose` and `apiPort` to `8080`.

Projects can extend that minimal contract with nested settings, builds, and secrets, as in this illustrative definition:

```nickel
let lib = import "libs/dockstride.ncl" in
let cfg = {
  project | String | doc "Unique lowercase project identity.",
  backend | lib.Backend | default = "compose",
  apiPort | lib.Port | doc "Published HTTP port." | default = 8080,
  imagePrefix | String | default = "sample",
  oauth = { enabled | Bool | default = false, issuer | String | default = "" },
  secrets = { authKey | lib.SecretSource },
} in
let env | cfg = import "env.yaml" in
let dc = lib.forEnvironment env in
dc.ComposeFile {
  dockstride | not_exported = {
    Config = cfg,
    setup.secrets.authKey = lib.GenerateSecret { bytes = 32, encoding = "hex" },
    endpoints.api = "http://localhost:%{env.apiPort}",
  },
  secrets = env.secrets,
  services.api = dc.Service {
    image = dc.image "api",
    build = "./api",
    user = "0:0",
    ports = ["%{env.apiPort}:8000"],
    secrets = dc.grantSecrets ["authKey"],
    environment = dc.Env { AUTH_KEY_FILE = dc.secretPath "authKey" },
  },
}
```

Nickel records are recursive: `Config = Config` would shadow an outer binding and recurse. Distinct outer names such as `cfg` avoid that ambiguity. Annotations precede field assignment/default values; `| doc ... | default = 8080` is valid, whereas appending `| doc` after `= 8080` is not.

Library interfaces: `Choice`, `Backend`, `Port`, `SecretSource`, `GenerateSecret`, `ReferenceSecret`, `PromptSecret`, `StdinSecret`, and `forEnvironment`, whose record provides `ComposeFile`, `Service`, `Env`, `image`, `grantSecrets`, and `secretPath`. `Env` converts scalars into Docker environment strings. `_FILE` handling is application code, not Docker magic.

The pinned library API is `0.3.0`, evaluated by embedded Nickel `0.19.0`. Keep the application's checked-in copy aligned with the CLI's embedded library; evaluation never fetches a newer snapshot.

The reserved `dockstride` record is non-exported. Its `Config` is queried independently before environment values exist. Supported reflection includes primitive fields, nested records, defaults/docs, choices, ports, and secret sources. Arbitrary function contracts remain Nickel's validation responsibility; their YAML values are not promised an automatically generated form.

The adapter pre-registers candidate `env.yaml` contents in memory; it never rewrites Nickel strings or temporarily changes the user's file. Complete execution validates the entire environment and canonical model. Partial setup permits filling independent fields, but replacing a valid complete environment with one missing required inputs is refused.

```sh
dks config list
dks config get oauth.enabled
dks config set oauth.enabled true
dks config set oauth --file /private/oauth.yaml
dks config unset apiPort
dks config edit
dks config schema --json
```

Block-YAML scalar edits preserve comments/order; replacing complex collections may regenerate only the affected record. Edits are locked, candidate-validated, and atomically replace one YAML document. `EDITOR` is split into argv, not implicitly executed by a shell. Secret fields accept validated direct file/external reference records, never plaintext; generated-secret replacement remains an explicit lifecycle operation. Project/backend changes are ordinary schema-validated edits and do not rename, adopt or remove existing Docker resources.

The illustrative secret consumer above declares numeric `user = "0:0"` so its private `0600` development secret is verifiably readable rather than assuming the image's default USER. The richer sample demonstrates UID-1000 consumers with explicit restrictive group access. The minimal initialized starter declares no secrets; these examples are not production-hardening claims.

### Named project commands

`dockstride.commands.NAME = { argv = ["python3", "scripts/command.py"], timeoutSeconds = 30 }` declares a trusted checkout command. `dks run NAME` executes it without applying returned configuration. Commands run with the pinned Docker connection, cancellation/process-group handling, bounded stderr diagnostics, and a JSON context on stdin. Successful stdout must be one JSON object containing `schemaVersion: 1`, at most 1 MiB. No implicit shell is invoked. Declared timeouts range from 1 to 300 seconds and are shortened by the invocation timeout.

Command discovery is lazy: missing service settings and credentials do not force operational metadata merely to locate a named command.

### Live shared settings and setup defaults

Local `env.yaml` may declare ordered settings/reference sources:

```yaml
_dockstride:
  sources:
    - path: ../main-checkout/env.shared.yaml
```

Sources resolve relative to their declaring file and may select other sources. Recursive mappings merge; scalars, lists, and explicit null replace entire values. Precedence is Config defaults, ordered sources, local overrides, then explicit invocation inputs. Shared `secrets.<name>` accepts validated absolute private file references or externally managed Swarm named refs, never raw credential bytes; generated policies cannot inherit credentials from shared sources. Cycles, missing declared files, unsafe references, and incompatible live settings fail explicitly. `_dockstride` is reserved and removed before applying the application contract or rendering Docker data.

```sh
dks config sources list
dks config sources add ../main-checkout/env.shared.yaml --create
dks config sources remove ../main-checkout/env.shared.yaml
dks config set oauth.enabled true --shared
dks config set oauth.issuer example --shared --source ../main-checkout/env.shared.yaml
dks config set secrets.apiToken '{file: /absolute/private/provider-token}' --shared
dks config set secrets.apiToken '{file: /absolute/private/checkout-token}'
dks config unset secrets.apiToken
dks config unset oauth.enabled
dks config edit --shared
```

Local edits change overrides, including direct secret references. Unsetting an override reveals current inheritance/defaults, not a saved shared copy. Shared edits target one direct source; multiple direct sources require `--source`, never an inferred transitive winner. The invoking checkout validates a shared edit; other checkouts validate the live update on their next command. List/get include winning file/path and overridden origins. Shared references remain in their original layer, not materialized into local `env.yaml`.

`setup.defaults = { command = "devDefaults", fields = ["project"], sources = true }` permits one named command to fill missing allowlisted ordinary settings and discover sources when no selection is declared. Input includes effective non-secret settings, provenance, selected paths, missing fields, and Dockstride's folder-derived `projectProposal`. The proposal lowercases the canonical checkout folder, replaces non-ASCII-alphanumeric characters with `-`, trims edge hyphens, uses `project` if empty, and caps the result at 40 characters. It never appends a path hash. Output is `{ "schemaVersion": 1, "values": { "project": "proposal" }, "sources": [{ "path": "/absolute/env.shared.yaml", "createIfMissing": true }] }`. Dockstride validates the entire proposal before create-new source publication; inherited/default/explicit/concurrently supplied values win. Accepted local values are persisted without flattening inherited settings.

`setup`, `up`, `dev`, and `deploy` may execute defaults; configuration reads, render, status, doctor, and `--plan` do not. Plans report whether discovery would run. Hook failure is explicit, not a fallback to guessed values. `dks setup --no-shared-sources` persists an explicit empty selection and disables discovery. Shared YAML and original externally owned credential files remain authoritative. A shared path edit changes the effective source on the next command, not a running consumer's mount; no automatic live credential refresh is performed.

Direct `secrets.<name>.file` references require secure absolute paths outside every repository. Shared ordinary scalar strings are not automatically rebased file paths.

Mutation lock order is checkout lifecycle, checkout config, then sorted canonical source-file guards. No operation acquires another checkout's lifecycle lock. Defaults, editors and trusted project actions run outside config/source guards; fingerprint revalidation rejects stale publication. Every edit replaces exactly one document. Explicit source creation uses atomic create-if-absent and cannot replace a concurrently created file; it is separate from publishing its local selection, not a multi-file transaction.

### Ownership and Git inventory

```sh
dks env list
```

Inventory lists only the invoking Git repository's discovered worktrees and their current checkout/configuration presence. It does not query Docker or evaluate Nickel, and has no saved cross-repository catalog or fallback. The result is `{schemaVersion:1,status:"available"|"not-git"|"unavailable",scope:"invoking-repository",worktrees:[{root,checkout,configuration}],error?:string}`; non-Git/unavailable observations return an empty list.

The configured project is the Compose project/Swarm stack namespace. Folder-derived proposals lowercase the canonical checkout basename, replace non-ASCII-alphanumeric characters with `-`, trim edge hyphens, use `project` if empty, and cap at 40 characters. Explicit/inherited names win; no UUID or hash suffix is added.

Managed resources carry `io.dockstride.owner=<canonical absolute UTF-8 checkout path>` and `io.dockstride.project=<effective project>`. A symlink to the same checkout has the same owner. Missing/foreign labels occupying the namespace or an explicitly managed name fail rather than authorize adoption. Explicit native external resources are neither claimed nor deleted. Unused historical secrets outside the current namespace/bindings do not reserve a stopped checkout.

Each invocation captures the current Docker connection and keeps it fixed until that operation exits. A later invocation may intentionally choose another target. Moving a checkout or changing project/backend does not transfer old ownership; reconcile or retire old resources manually. Opaque legacy UUID labels remain foreign. Ordinary setup and installation do not migrate or erase old credentials or metadata.

YAML/source files, external credential bytes and live Docker objects are the durable authorities. `.dockstride` holds only locks and disposable editor/render scratch. Remove it only between commands after locks are released; status/down/destroy need no ownership file or saved operation record.

### Ordinary allocated endpoints

Missing/null `setup.ports` fields receive one live-probed candidate during Compose setup/up/dev and become ordinary YAML values. Explicit non-null local/shared YAML wins; a schema default alone does not suppress assignment. Validate all services, targets and inclusive ranges before publishing one batch; keep probes alive until the write and avoid overlap with explicit or newly selected policy endpoints.

Policies retain service, target `1..65535`, host default `127.0.0.1`, `tcp|udp` default `tcp`, and inclusive default range `49152..65535`. Automatic allocation requires a local Unix Docker target. Remote targets require explicit ports. An exhausted range or invalid candidate leaves YAML unchanged. There is no reservation ledger, cross-invocation uniqueness guarantee, bind retry or occupied-value reroll; Docker remains the final bind authority.

Down/destroy preserve ordinary endpoints. Edit a normal field or deliberately unset an optional value to choose again; required-input validation still applies. The removed ports release/GC and env forget commands have no compatibility aliases.

## Lifecycle metadata

Native Compose healthchecks, `depends_on` conditions, one-shot services, and `develop.watch` are preferred. A small ordered action list fills gaps; it is not a workflow programming language.

```nickel
# Inside dockstride:
actions = [
  { name = "database", workflows = ["up", "dev"], kind = "up", service = "db" },
  { name = "stop-consumers", workflows = ["up", "dev"], services = ["api"],
    kind = "stop", targets = ["api", "worker"] },
  { name = "migrate", workflows = ["up", "dev"], services = ["api"],
    kind = "prerequisite", service = "migrate", fresh = true },
  { name = "seed", workflows = ["up", "dev"], services = ["api"],
    stage = "after", kind = "command", argv = ["./scripts/seed-development"] },
],
readiness.api = {
  url = "http://127.0.0.1:%{env.apiPort}/health",
  status = 200,
  json = { application = "sample", project = env.project },
},
```

Actions have `name`, `workflows`, optional applicability `services`, `stage` (`before`, default; or `after`), `kind` (`up`, `run`, `exec`, `command`, `stop`, `prerequisite`), and service/argv as applicable. Applicability includes the selected services' required dependency closure. Stop `targets` are separate from applicability: only actually running targets stop, and absent targets are never created. `command` executes project argv with no implicit shell.

A `prerequisite` requires a declared one-shot service, an explicit boolean `fresh`, and the `before` stage. With `fresh = true`, Dockstride builds/recreates a normal detached Compose service once for this invocation, waits for successful completion, and retains its container/logs. Remaining dependency-ordered startup uses `--no-deps` and revalidates that exact completed container instead of letting Compose rerun the prerequisite. Previously running stopped consumers, including out-of-scope consumers and affected restart relationships, resume only after verified success. Failure or cancellation does not resume them or run after-stage seeding; a detached prerequisite may still run after cancellation. Successful side effects and data are not rolled back.

Validate selections, required dependency cycles, conditions, native actions, and declared one-shots before stopping consumers. Optional `required = false` dependencies do not expand scope unless independently selected. One absolute lifecycle deadline covers actions, builds, dependency waits, and final HTTP/command readiness; it ends before the foreground development/watch loop. Prerequisite exit failures are operation errors with `details.prerequisite` containing safe container IDs/state/exit codes, not fabricated Docker process errors.

Readiness supports HTTP status, text `contains`, deep JSON-subset `json`, and/or `command` argv. `dockstride.oneshots = ["migrate"]` and required `service_completed_successfully` dependencies designate completion services; arbitrary exited-zero applications do not. Swarm production prerequisites require explicit deploy-workflow `command` actions; missing deploy applicability, Compose up/run/exec/stop/prerequisite, profiles, and `depends_on` are not production ordering guarantees. Secret replacement with `--apply` validates its workflow and service scope before publishing a new reference; a later rotation failure remains a committed replacement, not a rollback.

`dockstride.dev.argv = ["./scripts/develop"]` selects an explicit foreground development loop after readiness. Otherwise `dev` runs native Compose watch when declared; without either, it honestly reports detached operation rather than inventing synchronization. Cancellation leaves detached containers and completed side effects in place.

The rich sample explicitly declares native `rebuild` watch actions; the minimal initialized starter has no build or watch configuration. Changes rebuild/recreate the affected service rather than copying into its running filesystem. This is slower than `sync`/`sync+restart`, but avoids Docker rootless `fuse-overlayfs` archive-copy failures when read-only secret mounts cannot be remounted. Other declarations remain native Compose behavior; Dockstride never silently substitutes a watch action.

```nickel
setup.ports.apiPort = {
  service = "api", target = 8000,
  host = "127.0.0.1", protocol = "tcp",
  from = 49152, to = 65535,
},
```

Automatic allocation is explicit and local-target-only; selected values are persisted only in YAML. Setup/startup never moves an existing endpoint silently. Fixed occupied ports fail without stopping/reusing the occupying process. Probes prevent overlap only within this invocation; no durable reservation remains.

## Strict status and Compose profiles

```sh
dks up --profile debug
dks dev --profile debug --profile tools api
dks status
dks status api --profile debug
dks status --inspect-only
```

`status` observes once, not startup polling. One deadline covers Docker/context/ownership observations and application probes: 10 seconds by default, shortened by a smaller `--timeout`. Each reachable configured HTTP/command check runs once. Missing services, failed exits, OOM, unhealthy containers, unsuccessful prerequisites, wrong application identity, replica shortfall, and failed rollout produce an operation error.

Compose's default required scope is unprofiled services plus active-profile services and required dependencies. Explicit selections include their required dependency closure even when that service has an inactive profile. Repeatable flags and current `COMPOSE_PROFILES` form a union, including `*`; there is no saved-profile fallback. Inactive services remain visible as excluded. Unknown selections, dependency cycles, malformed declarations and zero required services are configuration errors.

Swarm observes current desired YAML and actual service/task specifications; a missing service is a missing/not-ready observation, not a missing deployment-record error. Read-only selection narrows scope without Compose profile/dependency semantics. Replica/mode/ownership and explicit digest references must match. Authored tags compare their normalized named reference before Docker's appended digest; this does not claim mutable-tag contents or current source freshness. Distinct current task slots and rollout state remain readiness requirements.

If Swarm retires a listed task before task inspection, the entire task collection is unobserved (`tasksObserved: false`), not a verified empty collection. Strict status does not claim native/application readiness from it, including zero-replica or one-shot cases. Deployment convergence re-observes under its existing deadline. Mixed errors, unrelated missing IDs, transport/permission failures, and malformed successful responses remain failures.

Reports distinguish `containerReady`, nullable `applicationReady`, required/excluded scope, and exhausted/unobserved checks. `--inspect-only` skips application probes and readiness-based failure, but genuine Docker/configuration/ownership errors still fail. Unprobed configured applications remain unverified (`applicationReady: null`, aggregate `ready: false`); inspection success does not claim application readiness.

Successful JSON status is one terminal result. Failed readiness is one operation error with the complete report in `details.status`; Docker failures retain their category and actual underlying process status plus available partial observations. Human output prints the same rows before the error. Status does not run defaults, allocate, provision credentials, or implicitly execute diagnostic hooks.


## Structured project diagnostics

Declare read-only hooks using named argv commands:

```nickel
commands.diagnosePostgres = {
  argv = ["python3", "scripts/diagnose-postgres.py"],
  timeoutSeconds = 10,
},
diagnostics.postgres = {
  command = "diagnosePostgres",
  services = ["postgres"],
  on = ["unhealthy", "readiness-failed", "startup-failed"],
},
```

Declarations are validated before managed resources start. After a startup failure, hook applicability includes the attempted dependency scope: a healthy database can explain a failed migration. `unhealthy` and `readiness-failed` use safe observed state, including the original application-probe result, without repeating application probes. Explicit `dks doctor` runs relevant hooks even when their automatic filters do not match; `dks doctor --plan` lists hooks without executing them. Status, render, and startup plans never dispatch diagnostic commands. Cancellation does not start a new diagnostic phase.

JSON stdin extends the common non-secret context with diagnostic triggers, hook/services, sanitized observations, failed services, secret references and primary failure kind. `owner` is the canonical checkout path. Container IDs require live path/project ownership; Swarm additionally requires exact immutable service/task/container linkage. Hooks inherit the invocation's captured Docker connection. No credential bytes, container environment, health logs, arbitrary task errors, private digests or keys are forwarded.

Hooks return one bounded JSON object:

```json
{"schemaVersion":1,"findings":[{"code":"postgres.auth.failed","severity":"error","summary":"Mounted credential cannot authenticate","evidence":{"database":"application"},"suggestedAction":"Inspect credential-volume mismatch before choosing explicit recovery"}]}
```

Severity is `info`, `warning`, or `error`. Findings require a stable nonempty code, summary, and evidence; optional `suggestedCommand` and `suggestedAction` remain inert data. Commands are trusted repository code under a read-only contract, not a sandbox: they must not repair credentials, recreate databases, or destroy resources.

One fresh diagnostic phase has a shared 10-second deadline, shortened by a smaller invocation timeout, covering context, ownership observations, and all hooks. This phase can run after the original startup deadline expires; it does not extend or restart startup. Capture is limited to 1 MiB. Malformed/duplicate JSON, nonzero exits, and timeouts become diagnostic failure entries. Startup retains its original category, exit code, Docker status, prerequisite details, and safe stop/seed behavior; findings attach at `details.diagnostics`. Doctor attaches them to its result. Human output shows findings and explicitly marks suggestions as not executed.

## Secrets

`Config.secrets.<logical-name>` uses `lib.SecretSource`. References are exactly `{file = "/absolute/private/key"}` or native Swarm `{external = true, name = "existing-secret"}`. Grants remain explicit and container filenames remain `/run/secrets/<logical-name>`. The records contain no credential bytes.

Initial policies:

```nickel
authKey = lib.GenerateSecret { bytes = 32, encoding = "hex" },
password = lib.PromptSecret,
apiToken = lib.ReferenceSecret,
importedKey = lib.StdinSecret,
```

Generation supports hex/base64 and 16–65536 source bytes. Nonempty inputs are bounded to 1 MiB. Every `--secret-file NAME=PATH` records the secure original file, never a managed copy, regardless of policy. Relative CLI paths resolve from the invocation directory. Existing references are reused and replacement inputs are ignored. Reference policies ask privately for a file path and reject stdin; prompt/stdin/generation without an original file create a new external private file. Only one missing secret may consume stdin.

File refs/storage must be outside the canonical checkout and every enclosing `.git` directory/file marker, including linked worktrees. Canonical existing ancestors, ownership/write restrictions and descriptor-relative no-follow access are checked; symlink traversal and unreadable ancestry fail closed. Files must be current-user-owned, nonempty regular private files. Missing/unreadable referenced bytes require restoring that exact file or explicit replacement, never regeneration/scanning.

### Private external storage

Default generated/input-byte files are `$XDG_DATA_HOME/dockstride/secrets/u<effective-uid>/<logical>--<random-hex>`, falling back to `$HOME/.local/share/dockstride/secrets/u<effective-uid>`. Absolute `dockstride.setup.secretDirectory` overrides the parent. New per-user directories are `0700`; compatible private current-owned directories, including non-listable ones, remain unchanged. No ownership marker or history is required, read, migrated or erased.

Files use exclusive atomic no-replace publication and `0600`, or declared restrictive owner/group access `0640`. Native Compose bind-backed secrets ignore requested container uid/gid/mode, so the declared numeric consumer must actually read the host file. Rootless UID/GID zero map to the invoking host user/group; `user = "1000:0"` requires explicit host-group `setup.secretAccess` and `0640`. Original files are never chmodded/chowned to fit a consumer. World-readable workarounds, remote/Desktop file portability and unknown userns mappings are refused.

The optional shared parent is an explicit administrator operation:

```sh
scripts/setup-secret-storage.sh --user APPLICATION_USER --directory /opt/secrets --plan
sudo scripts/setup-secret-storage.sh --user APPLICATION_USER --directory /opt/secrets
```

The script creates a root-owned `0755` parent and user-owned `0700` per-user directory without markers; incompatible existing directories fail rather than change. Normal CLI operations do not elevate.

### Current Swarm binding and replacement

```yaml
secrets:
  apiToken: {file: /absolute/private/provider-token}
  authKey: {file: /home/user/.local/share/dockstride/secrets/u1000/authKey--random}
_dockstride:
  sources: [{path: /absolute/shared-settings.yaml}]
  swarmSecrets:
    apiToken: dks-0123456789abcdef0123456789abcdef
    authKey: dks-fedcba9876543210fedcba9876543210
```

File references stay in their winning local/shared layer. Only current immutable object names live in the local binding map; shared maps are rejected and `_dockstride` is stripped from application settings. Generic setters cannot edit reserved bindings; the local editor validates them. Unused map entries are ignored by rendering. Generated refs remain local, never inherited.

Swarm rendering turns file-backed logical secrets into native external names from that map; missing bindings instruct provision/sync. Native external refs bypass managed publication. Initial provisioning saves a generated file reference before Docker, then creates/verifies a unique object labelled with checkout/project and `io.dockstride.secret=<logical>` before saving its binding. Ordinary setup validates existing source/object ownership without republishing changed contents. Missing/foreign current objects fail setup; explicit sync may republish an absent object from its valid source, never adopt a foreign one.

`secrets replace --file` switches directly to the new original; other replacement inputs write a fresh external file. Swarm creates/verifies its object before atomically switching source and binding in one local YAML document. Old files/objects are never automatically deleted, including on destroy.

### Explicit secret synchronization

```sh
dks secrets sync apiToken --plan
dks secrets sync apiToken --yes
dks secrets sync apiToken --yes --apply
```

Compose validates/reuses the current effective original file without copying or claiming a publication. Swarm always publishes a fresh immutable object, even with identical bytes, and changes only its local binding. Shared source YAML remains unchanged. Native external refs have no readable source and cannot sync. No HMAC, digest baseline, unchanged optimization, revision history or secret GC remains.

All selected metadata, source permissions, consumer access and declared rotations preflight before publication. `--plan` reads metadata but no credential bytes, writes no files/objects and consumes no stdin. `--apply` requires `setup.rotations.<name> = {workflow = "rotate-auth", services = ["api"]}` plus explicit applicable actions; Swarm accepts its existing command-action contract only. Release config/source guards before trusted actions.

Current list rows are `{name,backend,reference,binding,consumers,present}`, with null binding for Compose/native externals. Sync returns `{operation:"secrets-sync",sideEffects,secrets,committed,uncommitted,applied}`. Rows contain `{name,status,source:{canonicalPath,origin},reference,binding,consumers,applied,consumerRestartNeeded}` with `planned`, `validated` (Compose), `published` (Swarm) or `uncommitted`. Commit lists describe Swarm binding publications; Compose validation is not a fictitious commit.

Failed multi-name publication/application retains the original error and `details.secretSync` with exact completed publications and successful procedures. Committed refs/bindings remain authoritative after application failure; old material stays untouched. Storage replacement/restart does not rotate a database password or re-encrypt data. Consumers may cache bytes or retain an old bind mount: use declared rotation or explicit down/up rather than assuming live refresh.

Crash before file-reference commit can orphan a file; after commit retry reuses that exact file. Docker-create-before-binding failure reports its unbound object name for manual reconciliation. No orphan adoption, scanning, deletion, journal resume or file-backed rollback occurs.

## Full-stack Swarm deployment

Production runs on the selected Swarm manager. The full pipeline validates, builds/pushes authored registry image tags or pulls authored references, resolves this invocation's immutable digests, runs explicit deployment commands, applies and checks live convergence. Digest-bearing build targets fail before build. Explicit registry and multi-node distribution safeguards remain.

Pins preserve authored named tags (`repository:tag@sha256:...`, implicit `latest`); explicit digests remain immutable. Pins live only in the disposable full manifest and Docker service specifications. There is no saved image map, random revision tag, applied snapshot, intent or historical resume/selected-deploy API. Native Docker update/rollback declarations and prior live digests remain available.

The Swarm view drops development-only build/develop/depends_on/profiles/container_name/restart and keeps native deploy fields. Apply is `docker stack deploy --detach=true --with-registry-auth --compose-file <temporary> <project>`, never prune. Every declared service participates; return `{deployed,context,services,convergence}`. Failed rollouts leave actual Docker state inspectable.

Down/destroy enumerate and reinspect current owned immutable service/network/config IDs and volume fingerprints before deleting. Down preserves volumes/secrets; destroy additionally removes owned volumes and retains the multi-node volume refusal. Explicit external resources and all Swarm secrets remain untouched. Another privileged Docker writer can still race Docker's non-atomic named-volume deletion; no local ledger can eliminate that backend limitation.

Exact inspected network absence completes teardown without repeated deletion; permission/transport failures remain fatal.

## Machine interface

`--json` makes successful commands newline-delimited version-1 JSON. Long-running commands can emit any number of event records followed by one terminal record:

```json
{"schemaVersion":1,"type":"event","phase":"ready","message":"application readiness verified"}
{"schemaVersion":1,"type":"result","ok":true,"result":{"project":"sample"}}
```

Errors use `type:"error"`, `ok:false`, `category`, `exitCode`, `message`, and `details`. Missing ordinary/secret prompt inputs appear in `details.missingInputs`. Docker failures preserve `details.underlyingDockerStatus` and safe operation diagnostics. Secret-bearing errors redact raw/base64 bytes, not the entire failure explanation. Subprocess text is converted into events rather than mixed into JSON stdout.

Exit categories: `0` success; `1` operation failure; `2` usage/configuration; `3` prerequisite/scope; `4` Docker process failure; `5` consent required/declined; `130` cancellation. Built-in help/version/argument-error output follows Clap conventions. `--json completions SHELL` returns completion text in a result envelope; ordinary completions are raw shell text. Human native passthrough/exec/logs emits raw Docker stdout, no appended result document, and preserves interactive foreground terminal input.

`--plan` never generates secrets, consumes secret stdin/files, edits YAML, acquires mutation locks, builds, or deploys. It can read runtime state. Provided initial sources remain references in plans; unresolved required values are explicitly unresolved. Native passthrough does not support managed planning.

## Release and verification

```sh
cargo test --locked --all-targets
cargo build --locked --bin dks
scripts/smoke.py --dks target/debug/dks --compose
scripts/smoke.py --dks target/debug/dks --swarm --timeout 600
scripts/release.sh --container --verify-reproducible
```

Native host release mode requires the musl Rust target, musl-gcc/ar, readelf, GNU tar/gzip, and sha256sum. Container mode explicitly pulls/resolves an immutable Rust Alpine compiler digest and does not install host packages. Rootless compiler UID zero is the invoking host user, not host root. Static ELF checks reject interpreters/shared-library dependencies. Independent target directories verify byte-identical binaries; archive metadata/checksums/compiler provenance are deterministic.

The [GitHub workflow](https://github.com/MatthewScholefield/dockstride/actions/workflows/release.yml) builds native x86_64 and ARM64 runners automatically on main pushes and pull requests, using stable Rust and locked dependencies. Both architectures run tests, independent reproducibility checks, packaged evaluator smoke, and real HTTP installer acceptance before uploading artifacts. Version tags publish both archives/checksums only after both jobs pass; the tag version must match `Cargo.toml`. Branch artifacts do not replace the latest stable release.

```sh
curl -fsSL https://raw.githubusercontent.com/MatthewScholefield/dockstride/main/scripts/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
```

The installer supports Linux x86_64/ARM64, requires curl/tar/sha256sum and ordinary shell utilities, and defaults to `~/.local/bin/dks`. `DOCKSTRIDE_VERSION=v0.1.0` selects a specific release; `DOCKSTRIDE_INSTALL_DIR` changes the destination. It validates the exact archive checksum filename, extracts only the binary, checks the binary version, and atomically replaces it on the destination filesystem. Download/verification failures leave an existing binary unchanged. No sudo or Docker/host-package installation occurs. Checksums detect corruption; HTTPS and the repository publisher provide authenticity. For inspect-before-execute instructions, see the [README](../README.md#installation-and-prerequisites).

```sh
python3 scripts/smoke-install.py --archive dist/dockstride-0.1.0-x86_64-unknown-linux-musl.tar.gz
```

Installer acceptance serves real native archives over local HTTP, with only curl's URL origin redirected to the fixture. It exercises latest/pinned selection, paths with spaces, the installed evaluator, corrupt/misdirected checksums, missing downloads, wrong binary versions, and unsupported platforms. Failure cases prove the previous binary remains intact and staging/download state is cleaned.

Smoke uses real run-owned worktrees, canonical-path labels, migrations, HTTP identities, data persistence and secret-access proof. External generated/input storage is a sibling of checkouts under isolated XDG/HOME fixture roots; Docker's original configuration remains available. Choose a Git-free `TMPDIR` on a POSIX filesystem with sufficient capacity. A parent `.git` marker is deliberately rejected, not ignored for tests.

Swarm uses independent privileged Docker-in-Docker manager/worker/private-registry containers and contexts, never initializes the selected host's Swarm. Full build/push/digest updates, explicit sync/rotation, native rollback and live status/teardown are exercised against those daemons. Host/DIND privilege, bridge/overlay and registry availability are prerequisites; Compose-only proof cannot stand in for Swarm.

Cleanup verifies exact outer IDs/run labels/private mounts, independent daemon/cluster identities and complete disposable node membership. It retires only verified owned resources, including the verified worker before manager-only volume destruction. Unknown/unreachable identities or failed teardown retain recovery evidence. No generic prune, registry/reservation erasure or private host credential deletion runs.

The sample's `swarmDirectNetworking` defaults false. Isolated nested fixtures may explicitly use manager host-mode API publication and DNS round-robin instead of ingress/IPVS; their UUID-scoped outer network is the isolation boundary. That does not establish ingress routing-mesh coverage.

The pinned evaluator source includes a documented one-invariant upstream repair; [provenance](../vendor/nickel-lang-core/PATCHES.md) and executable nested-YAML/array regression remain in the repository. Evaluator/library upgrades require compatibility fixtures, not assumptions about reflection APIs.


### Observed minimal-state cutover verification

- `TMPDIR=/var/tmp cargo +1.96.0 test --all-targets`: 160 passed across 19 suites, with three ignored child-process helpers.
- `cargo +1.96.0 build --locked` and `cargo +1.96.0 install --path . --locked --force` passed. The installed optimized CLI was used for the Voxellum acceptance run.
- `TMPDIR=/var/tmp python3 scripts/smoke.py --dks target/debug/dks --compose --swarm --timeout 600` passed configuration/plan/Git-only inventory checks, real Compose worktrees and PostgreSQL diagnostics, fresh prerequisites, cancellation, direct-reference credentials, watch/data/readiness and scratch-independent teardown.
- The same combined smoke passed single/two-node full Swarm build/push/digest distribution and updates, native rollback, live status/replica faults, failed rollout/data retention, conservative multi-node destruction, shared original-file sync, failed explicit rotation with committed current binding, and independently verified disposable cleanup.
- Voxellum's six defaults-hook tests and three PostgreSQL-hook tests passed. Its installed-CLI E2E verified application readiness and the Chromium page-assistant scenario, then retired all run-owned resources and sibling storage.
- The selected host's Swarm remained inactive. The nested direct-network fixture does not establish ingress routing-mesh coverage; release reproducibility and installer claims below are historical, not rerun for this cutover.

### Historical 0.1.0 release verification

- 77 automated tests passed across eight suites. The one ignored helper is exercised as a child process by its parent test. Root-package Clippy with `-D warnings` passed; the vendored evaluator retains two upstream compiler warnings.
- Real fresh interactive startup and native TTY `exec` were exercised; the non-root sample reported UID 1000.
- Real Compose worktrees proved credential/port reuse, migration success/failure, application readiness, native watch rebuilds, Ctrl-C leaving detached containers usable, status/logs/exec, isolated checkouts, and owned down/destroy.
- The former Python starter (no longer generated by `dks init`) proved a changed `app/server.py` reaching its live HTTP response through native watch, followed by exit 130 on Ctrl-C with the endpoint still usable.
- Real single-node/repeated and two-node Swarm deployments proved immutable digest distribution to a worker, stable secret reuse, a selected API update leaving the unrelated worker specification/version unchanged, failed health rollout diagnostics, and owned teardown.
- Real private-file replacement proved explicit revision change; a running consumer kept its previous credential until an explicit restart. Missing-file setup refused regeneration without changing the reference/history; deliberate restoration reused the reference. GC deleted only an explicitly selected eligible old revision and protected the current revision.
- x86_64 musl release binaries were byte-identical across independent build directories. Static ELF checks, all packaged checksums, an independently byte-identical deterministic archive repack, and actual packaged schema/config/render plus PTY configuration editing passed.
- ARM64 musl release binaries and deterministic archives were byte-identical after a fresh independent compiler/cache/target build. Static ELF and embedded Nickel/config/render/setup were executed under standalone QEMU. Both XDG and HOME-fallback secret publication exercised ARM `SYS_renameat2`/`syncfs`, file mode 0600, directory mode 0300, and exact credential reuse using the real native Docker CLI.
- GitHub's native Ubuntu x86_64 and ARM64 main/tag runs passed tests, independent reproducibility, packaged evaluator checks, and real HTTP installer acceptance. The [v0.1.0 tag run](https://github.com/MatthewScholefield/dockstride/actions/runs/37201661956) published both architectures and checksum files automatically.
- The public README curl pipeline installed the actual GitHub v0.1.0 release into a disposable HOME; the installed binary reported `dks 0.1.0` and reflected nested configuration metadata correctly.

Local ARM execution used QEMU; hosted GitHub Actions separately verified native ARM64 execution. The direct Swarm fixture does not prove ingress routing mesh.
