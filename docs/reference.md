# Dockstride reference

## Configuration boundary

A project has one checked-in Nickel architecture and one editable environment mapping. The checked-in library is pinned and inspectable. `dks init` creates it locally without overwriting files; no evaluation-time fetch occurs.

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

Library interfaces: `Choice`, `Backend`, `Port`, `SecretSource`, `GenerateSecret`, `PromptSecret`, `FileSecret`, `StdinSecret`, and `forEnvironment`, whose record provides `ComposeFile`, `Service`, `Env`, `image`, `grantSecrets`, and `secretPath`. `Env` converts scalars into Docker environment strings. `_FILE` handling is application code, not Docker magic.

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

Block-YAML scalar edits preserve comments/order; replacing complex collections may regenerate only the affected record. Edits are locked, candidate-validated, and atomically published. `EDITOR` is split into argv, not implicitly executed by a shell. Secret fields do not accept plaintext through ordinary configuration commands. Project/backend identity edits serialize with lifecycle operations and refuse changes while recorded or live owned resources remain; a separate checkout is the normal production/development boundary.

The small starter declares numeric `user = "0:0"` so its private `0600` development secret is verifiably readable rather than assuming the image's default USER. The richer sample demonstrates UID-1000 consumers with explicit restrictive group access. The starter is not a production-hardening claim.

## Lifecycle metadata

Native Compose healthchecks, `depends_on` conditions, one-shot services, and `develop.watch` are preferred. A small ordered action list fills gaps; it is not a workflow programming language.

```nickel
# Inside dockstride:
actions = [
  { name = "database", workflows = ["up", "dev"], kind = "up", service = "db" },
  { name = "migrate", workflows = ["up", "dev"], services = ["api"],
    kind = "run", service = "migrate", argv = [] },
  { name = "seed", workflows = ["up", "dev"], services = ["api"],
    stage = "after", kind = "command", argv = ["./scripts/seed-development"] },
],
readiness.api = {
  url = "http://127.0.0.1:%{env.apiPort}/health",
  status = 200,
  json = { application = "sample", project = env.project },
},
```

Actions have `name`, `workflows`, optional selected `services`, `stage` (`before`, default; or `after`), `kind` (`up`, `run`, `exec`, `command`), and service/argv as applicable. `command` executes project argv with no implicit shell. Readiness can use HTTP status, text `contains`, deep JSON-subset `json`, and/or `command` argv. Runtime failures identify the service/phase; successful exited one-shots are not treated as crashed applications.

`dockstride.oneshots = ["migrate"]` can explicitly designate completion services; native `service_completed_successfully` dependencies are also recognized. Swarm production prerequisites require explicit deploy-workflow `command` actions; Compose up/run/exec and `depends_on` are not production ordering guarantees.

`dockstride.dev.argv = ["./scripts/develop"]` selects an explicit foreground development loop after readiness. Otherwise `dev` runs native Compose watch when declared; without either, it honestly reports detached operation rather than inventing synchronization. Cancellation leaves detached containers and completed side effects in place.

The starter and rich sample explicitly declare native `rebuild` watch actions. Changes rebuild/recreate the affected service rather than copying into its running filesystem. This is slower than `sync`/`sync+restart`, but avoids Docker rootless `fuse-overlayfs` archive-copy failures when read-only secret mounts cannot be remounted. Other declarations remain native Compose behavior; Dockstride never silently substitutes a watch action.

```nickel
setup.ports.apiPort = {
  service = "api", target = 8000,
  host = "127.0.0.1", protocol = "tcp",
  from = 49152, to = 65535,
},
```

Automatic allocation is explicit, local-context-only, locked, globally reserved per invoking user, and persisted in YAML/operation state. Setup/startup never moves an existing endpoint silently. Explicit ports win; fixed occupied ports fail without stopping/reusing the occupying process. Docker is the final authority on binding. Remote contexts require explicit port choices and cannot be checked by binding a local socket.

## Secrets

`Config.secrets.<logical-name>` uses `lib.SecretSource`. Docker-native sources are exactly `{file = "/absolute/private/revision"}` for Compose or `{external = true, name = "immutable-swarm-revision"}` for Swarm. Service grants are explicit and the container filename stays `/run/secrets/<logical-name>`.

Initial policies under `setup.secrets`:

```nickel
authKey = lib.GenerateSecret { bytes = 32, encoding = "hex" },
password = lib.PromptSecret,
apiToken = lib.FileSecret "/private/provider-token",
importedKey = lib.StdinSecret,
```

Generation supports hex/base64 and 16–65536 source bytes. Inputs are bounded to 1 MiB and nonempty. File input requires a private, current-user-owned regular file and rejects symlink traversal. Explicit initial `--secret-file NAME=PATH` or `--secret-stdin NAME` overrides a policy only for a missing reference; existing references are reused without reading supplied inputs. Relative CLI file paths are relative to invocation directory; relative declared file policies are relative to the project. Only one missing secret may consume stdin per invocation. Noninteractive prompts return structured missing inputs.

### Private file permissions

Default parent: `$XDG_DATA_HOME/dockstride/secrets` (normally `~/.local/share/dockstride/secrets`), current-user-owned/private, with ownership marker. Per-user `u<uid>` directory is mode `0300`; random revision filenames resist accidental discovery. Atomic exclusive creation uses anchored no-follow descriptors; file and directory-entry durability precede reference publication. An existing unmarked/incompatible store is refused, never commandeered.

The optional shared parent is an explicit administration operation:

```sh
sudo scripts/setup-secret-storage.sh --user APPLICATION_USER --directory /opt/secrets
# Non-mutating inspection of the intended host changes:
scripts/setup-secret-storage.sh --user APPLICATION_USER --directory /opt/secrets --plan
```

The parent is root-owned `0755`; `.dockstride-owner` is root-owned `0644` with the exact version marker; each `u<uid>` is user-owned `0300`. `dockstride.setup.secretDirectory` selects this path. Normal CLI operations never run wholesale as root. Configured history is used for normal listing/GC, not directory enumeration. Unknown/orphan files are not automatically adopted or deleted; administrative enumeration of non-listable storage requires separate elevated host inspection.

A root/non-root container must actually be able to read its granted file. Native Compose preserves host ownership/mode, ignoring long-form uid/gid requests for bind-backed secrets. `setup.secretAccess.<name> = {uid = HOST_UID, gid = HOST_GID}` explicitly authorizes restrictive group access (`0640`); default root-readable files stay `0600`. No world-readable credential workaround is used.

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

Replacement creates a new revision, atomically updates the reference, and retains the old one. Missing recorded files/objects fail rather than regenerate. Interrupted owned pending revisions reconcile through journals/labels. Externally managed references remain unowned; replacement never grants deletion authority over them.

`--apply --trust` requires `setup.rotations.<name> = {workflow = "rotate-auth", services = ["api"]}` plus explicit actions for that workflow. Credential rotation can fail after storage replacement; the CLI reports committed storage and retained previous revision without pretending database/encryption changes are transactional.

GC plans list exact `revision` identifiers and reasons for ineligibility. Actual deletion requires explicit identifiers and confirmation:

```sh
dks secrets gc --plan
dks secrets gc EXACT_REVISION_FROM_PLAN --yes
```

Current env references, retained snapshots, live Docker consumers, pending journals, or foreign ownership prevent deletion. GC and replacement serialize with lifecycle changes. `down` and data destruction preserve secret references. Snapshot retention is deliberately conservative; no blind orphan scanning or automatic cleanup runs at startup.

## Swarm deployment and selected scope

Production is server-local on a Swarm manager. Full pipeline: resolve/validate, build or pull, publish immutable built images, resolve digests, explicit prerequisites, apply, convergence, summary. Build information remains in the canonical model even though the Swarm view omits it.

Explicit adapters drop development-only `build`, `develop`, `depends_on`, `profiles`, `container_name`, and `restart`; stack output is version `3.8`. Unsupported infrastructure/security fields are diagnosed, not silently reinterpreted. Native deploy restart/update/rollback fields remain visible.

Selected apply supports image/environment/argv, user/workdir/hostname, read-only/init/TTY/stop settings, labels, network attachments, explicit bind/named-volume mounts, ports, secret/config grants, healthchecks, supported deploy settings, and logging. Unknown service/nested options, unsafe scalar removals, relative Swarm bind sources, and changes to shared resource definitions are rejected before build/apply with reviewed-full-deployment guidance. No implicit dependency deployment, omitted-service pruning, or all-consumer shared-secret update occurs.

Owned live service specifications are reconciled before selected updates. Durable pre-apply intent and applied checkpoints cover daemon acceptance before CLI acknowledgement, so retries remove unrecorded extra env/mount/network/secret/config grants. Only selected services are updated. Convergence reports failed/restarting/rejected tasks and update pause/rollback states; migrations are never automatically undone.

Destructive teardown uses ownership checks, pinned connections, immutable service/network/config IDs, and reinspection. Docker volumes have names rather than immutable IDs, so fingerprints are rechecked; there is no atomic compare-and-delete guarantee against another privileged Docker client. Multi-node destructive volume teardown is explicitly rejected. External resources are not commandeered or deleted.

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
scripts/smoke.py --dks target/debug/dks --swarm
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

The rich sample's `swarmDirectNetworking` defaults to false (normal ingress/VIP). The isolated rootless fixture explicitly sets it true: host-mode API publication on a manager and `deploy.endpoint_mode = "dnsrr"` avoid IPVS. Both disposable daemons explicitly enable userland proxy and create an ICC-enabled `docker_gwbridge`; their outer UUID network is the isolation boundary. Real two-node image/secret distribution and selected updates are verified with this direct profile. Ingress routing-mesh behavior is not claimed verified: this host's nested rootless namespace reports IPVS service creation `operation not permitted`. No host module change or ignored Docker netfilter error is used.

The pinned evaluator source includes a documented one-invariant upstream repair; [provenance](../vendor/nickel-lang-core/PATCHES.md) and executable nested-YAML/array regression remain in the repository. Evaluator/library upgrades require compatibility fixtures, not assumptions about reflection APIs.

### Observed 0.1.0 verification

- 77 automated tests passed across eight suites. The one ignored helper is exercised as a child process by its parent test. Root-package Clippy with `-D warnings` passed; the vendored evaluator retains two upstream compiler warnings.
- Real fresh interactive startup and native TTY `exec` were exercised; the non-root sample reported UID 1000.
- Real Compose worktrees proved credential/port reuse, migration success/failure, application readiness, native watch rebuilds, Ctrl-C leaving detached containers usable, status/logs/exec, isolated checkouts, and owned down/destroy.
- The initialized starter itself proved a changed `app/server.py` reaching its live HTTP response through native watch, followed by exit 130 on Ctrl-C with the endpoint still usable.
- Real single-node/repeated and two-node Swarm deployments proved immutable digest distribution to a worker, stable secret reuse, a selected API update leaving the unrelated worker specification/version unchanged, failed health rollout diagnostics, and owned teardown.
- Real private-file replacement proved explicit revision change; a running consumer kept its previous credential until an explicit restart. Missing-file setup refused regeneration without changing the reference/history; deliberate restoration reused the reference. GC deleted only an explicitly selected eligible old revision and protected the current revision.
- x86_64 musl release binaries were byte-identical across independent build directories. Static ELF checks, all packaged checksums, an independently byte-identical deterministic archive repack, and actual packaged schema/config/render plus PTY configuration editing passed.
- ARM64 musl release binaries and deterministic archives were byte-identical after a fresh independent compiler/cache/target build. Static ELF and embedded Nickel/config/render/setup were executed under standalone QEMU. Both XDG and HOME-fallback secret publication exercised ARM `SYS_renameat2`/`syncfs`, file mode 0600, directory mode 0300, and exact credential reuse using the real native Docker CLI.

ARM execution here is cross-architecture emulation, not native ARM hardware. Native ARM CI is configured but was not run locally. The direct Swarm fixture does not prove ingress routing mesh.
