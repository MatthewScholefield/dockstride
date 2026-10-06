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

Library interfaces: `Choice`, `Backend`, `Port`, `SecretSource`, `GenerateSecret`, `ReferenceSecret`, `PromptSecret`, `FileSecret`, `StdinSecret`, and `forEnvironment`, whose record provides `ComposeFile`, `Service`, `Env`, `image`, `grantSecrets`, and `secretPath`. `Env` converts scalars into Docker environment strings. `_FILE` handling is application code, not Docker magic.

The pinned library snapshot is version `0.2.0`, evaluated by embedded Nickel `0.19.0`. Keep the application's checked-in copy aligned with the CLI's embedded library; evaluation never fetches a newer snapshot.

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

Block-YAML scalar edits preserve comments/order; replacing complex collections may regenerate only the affected record. Edits are locked, candidate-validated, and atomically published. `EDITOR` is split into argv, not implicitly executed by a shell. Secret fields accept validated direct file/external reference records through configuration commands, never plaintext; generated-secret replacement remains a separate lifecycle rather than a config-edit bypass. Project/backend identity edits serialize with lifecycle operations and refuse changes while recorded or live owned resources remain; a separate checkout is the normal production/development boundary.

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

Local edits change overrides, including direct secret references. Unsetting an override reveals the current inheritance/defaults, not a saved shared copy. Shared edits target one direct source; multiple direct sources require `--source`, never an inferred transitive winner. The invoking checkout validates a shared edit; other checkouts validate the live update on their next command. List/get include winning file/path and overridden origins. Existing scalar YAML comments are preserved. Shared Compose references are used directly by setup, list, doctor, consumer access checks, and GC protection without being materialized into local `env.yaml`.

`setup.defaults = { command = "devDefaults", fields = ["project"], sources = true }` permits one named command to fill missing allowlisted ordinary settings and discover sources when no selection is declared. Input includes effective non-secret settings, provenance, selected paths, missing fields, and Dockstride's folder-derived `projectProposal`. The proposal lowercases the canonical checkout folder, replaces non-ASCII-alphanumeric characters with `-`, trims edge hyphens, uses `project` if empty, and caps the result at 40 characters. It never appends a path hash. Output is `{ "schemaVersion": 1, "values": { "project": "proposal" }, "sources": [{ "path": "/absolute/env.shared.yaml", "createIfMissing": true }] }`. Dockstride validates the entire proposal before create-new source publication; inherited/default/explicit/concurrently supplied values win. Accepted local values are persisted without flattening inherited settings.

`setup`, `up`, `dev`, and `deploy` may execute defaults; configuration reads, render, status, doctor, and `--plan` do not. Plans report whether discovery would run. Hook failure is explicit, not a fallback to guessed values. `dks setup --no-shared-sources` persists an explicit empty selection and disables discovery. Shared YAML and original externally owned credential files remain authoritative. A shared path edit changes the effective source on the next command, not a running consumer's mount; no automatic live credential refresh is performed.

Direct `secrets.<name>.file` references require absolute paths. Shared ordinary scalar strings are not automatically rebased file paths. Separately, a declared `FileSecret` managed-import policy still permits project-relative input paths; that policy intentionally creates an owned immutable copy rather than treating its input as the deployed Compose source.

Mutation lock order is checkout lifecycle, per-user environment/allocation coordination, local allocation when required, canonical configuration/source files in sorted path order, then individual state files. No operation acquires another checkout's lifecycle lock under the global lock. Defaults and editors run outside publication locks; fingerprint revalidation prevents stale editor output from replacing a concurrent update. Allocation and secret publication use internal already-locked helpers rather than reacquiring advisory locks.

Ordinary multi-file settings/allocation transitions use a private versioned pending-publication journal. Intended claims are recorded globally before local files; registry commits come last. Interrupted mutations resume only when every file matches its recorded before/after fingerprint. External edits produce an actionable conflict and retain conservative pending blockers, never an overwrite or implicit release. Read-only commands and plans report pending paths/claims without recovery writes. Managed mutations recover before executing defaults.


### Registered environments

```sh
dks env list
dks env list --worktrees
dks env forget /absolute/checkout --plan
dks env forget /absolute/checkout --yes
```

Complete managed setup registers even stopped environments under `$HOME/.local/share/dockstride/.dockstride/`. Project claims are scoped by the verified Docker daemon ID, not just a context name: aliases cannot let another checkout claim the same project. Ownership checks still reject foreign Docker resources. Registry entries retain checkout/owner identity, backend/project, pinned connection, daemon ID, source paths, and endpoints. A moved checkout is not an ownership transfer.

The configured project is the actual Compose project/Swarm stack namespace. Folder-derived proposals are also offered by interactive setup. Equal folder names can propose equal namespaces; a same-daemon collision fails and requires an explicit readable `project` override rather than an automatic suffix. Existing configured names are not rewritten, and there is no automatic resource migration.

A successful full Compose `destroy` retires the deployment identity even when credential files are retained. After removing the old containers, networks, and volumes, a new project can be configured in the same checkout, including after moving `env.yaml` aside. Secret provisioning verifies previous owned Docker resources are absent before accepting the retired namespace change; owner, checkout, backend, and Docker-context safeguards remain. `down`, scoped teardown, and failed destroy do not grant this transition.

Listing reads saved records without Docker or Nickel evaluation and reports missing, stale, unreadable, and pending entries. `--worktrees` augments the invoking Git repository's worktrees, including unregistered configuration presence; non-Git registration/listing remain supported. Arbitrary historical directories are not retroactively discovered. Older configured checkouts register on their next successful managed command.

Forgetting removes only registration, never checkout files or private credential revisions. Stopped containers, networks, volumes, owned Swarm objects, pending publications, and reservations block it. An unreachable recorded connection cannot prove resource absence. Plans list exact blockers and perform no writes.

### Generated endpoints and explicit reset

```sh
dks ports release --plan
dks ports release --yes
dks ports gc --plan
dks ports gc --yes
```

Declared native port policies allocate checkout-local values during setup, including required Config port fields. Inherited and explicit local values win over automatic allocation. Allocation metadata records the reservation key, generated/explicit provenance, owner, verified daemon, and pinned connection. Global `port-reservations.json` and local `ports.json` use `schemaVersion: 1`, with `reservations` and `allocations` mappings respectively.

Normal `down` and `destroy` retain endpoints and credentials. Release is the explicit endpoint reset: its plan shows exact keys, generated fields, preserved overrides, protected reservations, and Docker blockers. Plans never acquire mutation locks, execute defaults, read credential contents, or allocate. Running or stopped owned containers and Swarm service publications block release; retained volumes, networks, and unused credential files do not themselves consume endpoints. Recorded-target failures retain claims rather than treating an unreachable daemon as empty.

Release publishes reservation removal, local metadata, generated YAML removal, and registry endpoints through the recoverable journal. It removes only a still-generated local value matching its allocation. Intentional local edits, including setting the same value, mark that field explicit; shared writes do not change local generated provenance. Releasing an explicit override retains its YAML value while freeing the old generated reservation. Removing a generated local override can reveal a shared/default value without copying it locally.

Next setup allocates eligible missing fields again; it may reuse a freed port. Existing unversioned global maps and integer local allocations remain readable. Only a matching historical saved local allocation can be reconciled as generated; mismatches are conflicts, not permission to erase settings. Unlinked legacy reservations remain protected.

GC collects only absent-checkout or retired-registration reservations with sufficient recorded owner/target evidence and verified resource absence. It never locks or edits another checkout's local files. Legacy records without that evidence and unreachable or replaced daemons fail closed.

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

Automatic allocation is explicit, local-context-only, locked, globally reserved per invoking user, and persisted in YAML/operation state. Setup/startup never moves an existing endpoint silently. Explicit ports win; fixed occupied ports fail without stopping/reusing the occupying process. Docker is the final authority on binding. Remote contexts require explicit port choices and cannot be checked by binding a local socket.

## Strict status and Compose profiles

```sh
dks up --profile debug
dks dev --profile debug --profile tools api
dks status
dks status api --profile debug
dks status --inspect-only
```

`status` observes once, not startup polling. One deadline covers Docker/context/ownership observations and application probes: 10 seconds by default, shortened by a smaller `--timeout`. Each reachable configured HTTP/command check runs once. Missing services, failed exits, OOM, unhealthy containers, unsuccessful prerequisites, wrong application identity, replica shortfall, and failed rollout produce an operation error.

Compose's default required scope is unprofiled services plus active-profile services and their required dependencies. Explicit service selection includes its required dependency closure even when that service has an inactive profile. Repeatable flags and `COMPOSE_PROFILES` form a union, including `*`; an explicitly empty environment selection suppresses saved-profile fallback. Startup without a profile selection uses none. Only unqualified status falls back to the last applied profiles; the scope is recorded after successful resource application, before application readiness, and survives teardown or failures before application. Inactive services remain visible as excluded. Unknown selections, dependency cycles, malformed declarations, and zero required services are configuration errors.

Swarm observes the accumulated applied service scope, not merely the last selected update; without a deployment it observes desired configured services and reports the missing deployment. Explicit selection narrows that scope. It does not acquire Compose profiles or dependency semantics. Compare owned service/task IDs, applied image revisions, desired replicas, distinct current task slots, and rollout state. Declared one-shots require successful expected completed tasks; deployment convergence and status share that rule.

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

JSON stdin extends the common non-secret command context with `diagnostics`: trigger/triggers, hook, services, sanitized observations, failed services, secret references, and the primary failure kind. Service observations carry ownership-verified container IDs; Swarm additionally requires immutable service/task/container linkage. A saved registration's Docker daemon and connection must still match. Native secret references identify source, mounted target, and file path or external name; no credential bytes, Docker environment, health logs, private digests, or keys are forwarded. Commands inherit the captured Docker connection.

Hooks return one bounded JSON object:

```json
{"schemaVersion":1,"findings":[{"code":"postgres.auth.failed","severity":"error","summary":"Mounted credential cannot authenticate","evidence":{"database":"application"},"suggestedAction":"Inspect credential-volume mismatch before choosing explicit recovery"}]}
```

Severity is `info`, `warning`, or `error`. Findings require a stable nonempty code, summary, and evidence; optional `suggestedCommand` and `suggestedAction` remain inert data. Commands are trusted repository code under a read-only contract, not a sandbox: they must not repair credentials, recreate databases, or destroy resources.

One fresh diagnostic phase has a shared 10-second deadline, shortened by a smaller invocation timeout, covering context, ownership observations, and all hooks. This phase can run after the original startup deadline expires; it does not extend or restart startup. Capture is limited to 1 MiB. Malformed/duplicate JSON, nonzero exits, and timeouts become diagnostic failure entries. Startup retains its original category, exit code, Docker status, prerequisite details, and safe stop/seed behavior; findings attach at `details.diagnostics`. Doctor attaches them to its result. Human output shows findings and explicitly marks suggestions as not executed.

## Secrets

`Config.secrets.<logical-name>` uses `lib.SecretSource`. Docker-native sources are exactly `{file = "/absolute/private/key"}` for Compose or `{external = true, name = "swarm-secret-name"}` for Swarm. Service grants are explicit and the container filename stays `/run/secrets/<logical-name>`. These records contain references, not credential bytes; validated direct refs can be set/unset/edited locally or in shared YAML.

Initial policies under `setup.secrets`:

```nickel
authKey = lib.GenerateSecret { bytes = 32, encoding = "hex" },
password = lib.PromptSecret,
apiToken = lib.ReferenceSecret,
managedToken = lib.FileSecret "/private/provider-token",
importedKey = lib.StdinSecret,
```

Generation supports hex/base64 and 16–65536 source bytes. Inputs are bounded to 1 MiB and nonempty. File input requires a private, current-user-owned regular file and rejects symlink traversal. Initial `--secret-file NAME=PATH` supplies a missing reference; existing references are reused without reading a replacement input. Relative CLI paths resolve from the invocation directory.

`lib.ReferenceSecret` is `{kind = "reference"}`. For a missing reference, initial `--secret-file` or a private interactive **file-path** prompt records the validated absolute original path. It never collects plaintext and rejects `--secret-stdin`. Compose directly mounts the original file, without writing provider bytes to managed storage or saving inherited shared refs locally. An existing externally owned file remains external: setup, replace, sync, GC, and destroy never delete, chmod, or chown it. The operator must arrange safe consumer readability; Dockstride validates access rather than silently changing it.

`FileSecret`, `PromptSecret`, `StdinSecret`, and explicit import APIs intentionally provision managed immutable copies. For these policies, initial `--secret-file`/`--secret-stdin` can override the missing input policy; relative declared file-policy paths resolve from the project. Only one missing secret may consume stdin per invocation. Noninteractive prompts report missing inputs. This managed-copy lifecycle is distinct from reference policies, not a fallback or compatibility shim.

Generated policies stay deployment-local and retain their existing revisions; shared refs cannot supply generated credentials. Native config commands validate direct refs and do not permit raw values or replacement of generated secrets outside their lifecycle.

### Private file permissions

For generated credentials and intentionally managed imports, the default parent is `$XDG_DATA_HOME/dockstride/secrets` (normally `~/.local/share/dockstride/secrets`), current-user-owned/private, with ownership marker. Per-user `u<uid>` directory is mode `0300`; random revision filenames resist accidental discovery. Atomic exclusive creation uses anchored no-follow descriptors; file and directory-entry durability precede reference publication. An existing unmarked/incompatible store is refused, never commandeered. Reference-backed Compose credentials are not copied here.

The optional shared parent is an explicit administration operation:

```sh
sudo scripts/setup-secret-storage.sh --user APPLICATION_USER --directory /opt/secrets
# Non-mutating inspection of the intended host changes:
scripts/setup-secret-storage.sh --user APPLICATION_USER --directory /opt/secrets --plan
```

The parent is root-owned `0755`; `.dockstride-owner` is root-owned `0644` with the exact version marker; each `u<uid>` is user-owned `0300`. `dockstride.setup.secretDirectory` selects this path. Normal CLI operations never run wholesale as root. Configured history is used for normal listing/GC, not directory enumeration. Unknown/orphan files are not automatically adopted or deleted; administrative enumeration of non-listable storage requires separate elevated host inspection.

A root/non-root container must actually be able to read its granted file. Native Compose preserves host ownership/mode, ignoring long-form uid/gid requests for bind-backed secrets. For owned managed storage, `setup.secretAccess.<name> = {uid = HOST_UID, gid = HOST_GID}` explicitly authorizes restrictive group access (`0640`); default root-readable files stay `0600`. External source files are checked as supplied, never chmodded/chowned to fit a consumer. No world-readable credential workaround is used.

Rootless local Docker maps container UID/GID zero to the invoking host user/group. A non-root container `user = "1000:0"` can read an explicitly authorized invoking-host-group `0640` file. The rich sample exposes `hostSecretUid`, `hostSecretGid`, and `containerGid` for this policy. Remote hosts, Docker Desktop mounts, and unknown userns-remap mappings are refused by the host-file backend rather than claimed portable. Swarm secrets use Docker's native distribution instead.

Mode `0300` is accidental-disclosure resistance, not a sandbox against malicious same-user code: the owner can change permissions and privileged/known-path readers can still access bytes.

### Production recovery and replacement

Swarm secrets are immutable and scoped to the pinned Docker context/cluster. Generated durable production credentials require an exclusive private recovery source **before** bytes are sent to Docker:

```nickel
setup.secrets.authKey = lib.GenerateSecret {
  bytes = 32, encoding = "hex", durable = true,
  recoveryFile = "/private/backups/authKey-{revision}",
},
```

The absolute recovery path contains literal `{revision}` and has a private `0700` parent; revision backups are exclusive `0600` and fsynced. `durable = false` explicitly declares an ephemeral/recreatable credential. Swarm inspect cannot return plaintext, and a new cluster cannot reconstruct generated credentials from a reference.

Managed replacement creates a new revision, atomically updates the reference, and retains the old one. Missing recorded files/objects fail rather than regenerate. Interrupted owned pending revisions reconcile through journals/labels. Externally managed references remain unowned; replacement never grants deletion authority over their files/objects. Reference-backed Compose path changes keep a direct original-file reference, rather than importing bytes into an owned revision.

`--apply` requires `setup.rotations.<name> = {workflow = "rotate-auth", services = ["api"]}` plus explicit actions for that workflow. Credential rotation can fail after storage replacement; the CLI reports committed storage and retained previous revision without pretending database/encryption changes are transactional.

Reference-backed Swarm providers have a different copy boundary: Docker receives the bytes as an immutable secret, but Dockstride does not create a redundant private host recovery copy. The original file remains authoritative. A local Docker revision reference may be recorded for deployment while the shared file source remains live; explicit sync/rotation uses the current effective source. Ordinary startup reuses the published Docker revision and never silently refreshes changed source content. An externally managed `{external = true, name = "existing-secret"}` reference is reused without adopting Docker deletion authority. None of this relaxes recovery requirements for generated durable Swarm credentials.


### Explicit imported-secret synchronization

Reference-backed Compose uses the current original file/path directly; sync does not materialize a managed host copy. Reference-backed Swarm reads the current effective file source on explicit sync and publishes changed bytes only into Docker's immutable storage. File imports separately record their canonical input path and origin privately with the immutable owned revision; ordinary setup/startup reuses that deployed revision even when the input changes. There is no automatic live credential refresh. Synchronization is explicit and names every selected secret:

```sh
dks secrets sync apiToken --plan
dks secrets sync apiToken --yes
dks secrets sync apiToken --yes --apply
```

For reference policies, the current effective direct file ref is authoritative, including live shared path changes rather than an older local Docker revision's origin. For managed imports, the current declared `FileSecret` takes precedence; otherwise sync uses the current revision's explicit CLI-file origin. A stdin replacement does not inherit an older file origin. Source settings are never rewritten. Files must remain private, current-user-owned regular files; symlinks, empty/oversized inputs, and unavailable origins fail before publication. All selected sources and required rotation declarations are checked before the first replacement.

Unchanged comparable inputs reuse their revisions. Private history stores an HMAC-SHA-256 digest, keyed by a durable owner-only `0600` key at `$HOME/.local/share/dockstride/.dockstride/secret-source-key`; neither digests, key material, nor credential bytes enter results. Legacy Compose revisions can establish a baseline by securely comparing their owned managed file. A legacy Swarm revision cannot prove content equality: sync creates a new immutable revision and reports `priorContentComparable: false`. Plans inspect source metadata without reading credential contents or creating a key, and report `comparison-deferred`, never a speculative unchanged/changed result.

Without `--apply`, changed storage reports `consumerRestartNeeded: true`. An ordinary Compose `up` can retain an existing bind-backed secret mount; use the project's declared rotation procedure or explicit `down`/`up` after changing source paths or replacing files. In-place writes can be visible through a mount, but applications may cache credentials; no automatic reload is promised. Swarm revision changes likewise require explicit consumer remount/rotation. Storage/reference replacement alone does not rotate a database password or re-encrypt stored data. Unchanged `--apply` skips rotation. Failures retain the original error and `details.secretSync` with exact committed, uncommitted, and successfully applied names; already published references and previous revisions are retained.

### Protected revision GC

Registered checkouts' effective local/shared direct file refs, declared managed-import inputs, current revision file origins, current/pending references, and retained operation/deployment snapshots protect matching owned paths and inode aliases. Live consumer mounts and Swarm service grants remain blockers. Externally owned original files are never GC targets; using a Dockstride-owned revision as a shared source does not revoke its ownership or retention checks. GC observes foreign checkout metadata without acquiring foreign lifecycle locks or enumerating mode-`0300` credential stores, and revalidates its evidence before each deletion. Missing, moved, unreadable, unsafe, or externally changed evidence fails closed. Explicit registry forget removes that checkout's protection only after its owned resources have been cleared.

GC plans list exact `revision` identifiers and reasons for ineligibility. Actual deletion requires explicit identifiers and confirmation:

```sh
dks secrets gc --plan
dks secrets gc EXACT_REVISION_FROM_PLAN --yes
```

Current env references, retained snapshots, live Docker consumers, pending journals, or foreign ownership prevent deletion. GC and replacement serialize with lifecycle changes. `down` and data destruction preserve secret references. Snapshot retention is deliberately conservative; no blind orphan scanning or automatic cleanup runs at startup.

Completed deployment journals also retain the operation's previous/planned/applied secret snapshots. Successful down does not erase that historical protection. The disposable smoke explicitly retires entire verified inactive, reference-bearing deployment journals; it does not rewrite historical events, alter revision history, disable GC protections, or expose a public snapshot-retirement command.

## Swarm deployment and selected scope

Production is server-local on a Swarm manager. Full pipeline: resolve/validate, build or pull, publish immutable built images, resolve digests, explicit prerequisites, apply, convergence, summary. Build information remains in the canonical model even though the Swarm view omits it.

Explicit adapters drop development-only `build`, `develop`, `depends_on`, `profiles`, `container_name`, and `restart`; stack output is version `3.8`. Unsupported infrastructure/security fields are diagnosed, not silently reinterpreted. Native deploy restart/update/rollback fields remain visible.

Selected apply supports image/environment/argv, user/workdir/hostname, read-only/init/TTY/stop settings, labels, network attachments, explicit bind/named-volume mounts, ports, secret/config grants, healthchecks, supported deploy settings, and logging. Unknown service/nested options, unsafe scalar removals, relative Swarm bind sources, and changes to shared resource definitions are rejected before build/apply with reviewed-full-deployment guidance. No implicit dependency deployment, omitted-service pruning, or all-consumer shared-secret update occurs.

Owned live service specifications are reconciled before selected updates. Durable pre-apply intent and applied checkpoints cover daemon acceptance before CLI acknowledgement, so retries remove unrecorded extra env/mount/network/secret/config grants. Only selected services are updated. Convergence reports failed/restarting/rejected tasks and update pause/rollback states; migrations are never automatically undone.

Destructive teardown uses ownership checks, pinned connections, immutable service/network/config IDs, and reinspection. Docker volumes have names rather than immutable IDs, so fingerprints are rechecked; there is no atomic compare-and-delete guarantee against another privileged Docker client. Multi-node destructive volume teardown is explicitly rejected. External resources are not commandeered or deleted.

Previously verified owned networks are removed by captured immutable ID. Exact backend absence completes teardown only once inspection proves the object is gone. If removal reports absence while a local inspection still sees the object, teardown stops sending deletion requests and continues ownership-checked inspection under the existing deadline. Permission/transport failures remain fatal; named-volume recreation fingerprints remain mandatory.

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

Smoke fixtures create real worktrees, containers, secrets, migrations, HTTP identities, watch-driven rebuilds, and failures. Compose uses UUID-labelled resources on the selected daemon. Both `HOME` and XDG storage roots are private fixture paths; the original Docker configuration remains available for contexts/credentials. Swarm uses separate disposable Docker-in-Docker manager/worker/registry containers and contexts, never initializes the original daemon, and verifies actual worker image/secret distribution before claiming multi-node readiness. Infrastructure failure is a failing check, not a silently skipped test. Cleanup is ownership/UUID scoped.

The DIND daemons keep their data in bind-mounted, run-owned directories under the fixture's temporary root. Set `TMPDIR` to a private directory on a POSIX filesystem with enough capacity when the host Docker filesystem is constrained. Cleanup stops the disposable daemons before removing mapped-UID files through an isolated helper; it never prunes host Docker storage. Successful runs retire owned resources, release reservations, forget registrations, verify zero remaining claims, and remove their temporary storage. Failed runs retain evidence. If native teardown fails, the pinned DIND contexts/daemons and private configuration are retained for explicit recovery instead of hiding leaked claims.

Failure cleanup verifies exact outer container IDs/UUID labels, private mounts, pinned daemon/cluster identities, and the complete disposable node membership before mutation. It downs registered owned services and retires only the verified disposable worker before native manager-only destroy. Unknown or unreachable nodes retain recovery state; the product's conservative multi-node volume-deletion refusal is not bypassed.

The rich sample's `swarmDirectNetworking` defaults to false (normal ingress/VIP). The isolated rootless fixture explicitly sets it true: host-mode API publication on a manager and `deploy.endpoint_mode = "dnsrr"` avoid IPVS. Both disposable daemons explicitly enable userland proxy and create an ICC-enabled `docker_gwbridge`; their outer UUID network is the isolation boundary. Real two-node image/secret distribution and selected updates are verified with this direct profile. Ingress routing-mesh behavior is not claimed verified: this host's nested rootless namespace reports IPVS service creation `operation not permitted`. No host module change or ignored Docker netfilter error is used.

The pinned evaluator source includes a documented one-invariant upstream repair; [provenance](../vendor/nickel-lang-core/PATCHES.md) and executable nested-YAML/array regression remain in the repository. Evaluator/library upgrades require compatibility fixtures, not assumptions about reflection APIs.

### Observed unreleased workflow verification

- Initial `cargo test --locked --all-targets`: 203 passed across 21 suites. Final `cargo test --locked --release --all-targets`: 205 passed across 21 suites, with three ignored child-process helpers exercised by their parent tests. The optimized CLI build and normal-PATH installation passed.
- The real Compose fixture proved stopped shared-source worktrees retain distinct registrations/ports, exact strict identity and unhealthy/missing reports, fresh migration once per startup, exit-23/cancellation preservation of stopped consumers and database data, and explicit release/forget/recreate isolation.
- Actual file-secret synchronization proved unchanged-input no-op, explicit changed revision publication, old live/snapshot/source-path protection, and eligible old-revision GC.
- Real PostgreSQL startup diagnostics distinguished initialization, a missing database, and an incorrect mounted password through read-only service-network connections; original failures and stored role/data survived recovery.
- The isolated Voxellum stack preserved its account and database sentinel across repeated startup and down/up, rejected a deliberately failed migration without restarting consumers, and recovered after restoring the declared command. The evaluation environment loader observed live shared preferences and local overrides. The existing Chromium page-assistant E2E passed, followed by owned resource, registry, and reservation retirement.
- A real selected two-node rollout with aggressive task-history retirement verified the bounded task-observation fix, exact new HTTP revision, strict readiness, and the retained application-volume sentinel. The early-failed multi-node fixture then retired its verified worker and completed native/outer/private-storage cleanup with zero claims.
- Real old-revision Swarm GC remained blocked by terminal deployment-journal snapshots after down; explicit verified fixture-only journal retirement made the selected old revision eligible without changing the committed revision/history. Native down and complete retained-fixture cleanup passed after the conservative network-absence fix.
- The complete final disposable single-node/two-node Swarm harness passed: immutable digest distribution, repeated/scoped deployment, strict native/application readiness and fault restoration, failed-rollout data retention, conservative multi-node destroy, private file-secret synchronization and committed failed-apply retention, live/snapshot GC protection, eligible old-revision retirement, and complete owned resource cleanup. The original host daemon remained outside Swarm.

### Observed 0.1.0 verification

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
