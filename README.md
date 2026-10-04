# Dockstride

[![Linux builds](https://github.com/MatthewScholefield/dockstride/actions/workflows/release.yml/badge.svg)](https://github.com/MatthewScholefield/dockstride/actions/workflows/release.yml)

Dockstride is a transparent development and deployment CLI for Docker applications. One checked-in application definition supports a configured Compose development environment and a separately configured Swarm production environment. `dks` owns setup, secret references, startup, readiness, watch, image publication, scoped deployment, and safe teardown—not the entire host.

## The project contract

```text
compose.ncl              Nickel architecture, Config contract, operational metadata
libs/dockstride.ncl       Checked-in pinned Dockstride library
env.yaml                 Local environment values and secret references; ignored
.dockstride/             Private locks, ownership, journals, snapshots; ignored
```

One directory represents one environment and identity. Development and production use different directories/checkouts. There are no hidden environment overlays. YAML contains references, never secret bytes. Configuration edits do not restart containers or deploy services.

Nickel supplies types, documentation, defaults, reusable definitions, and validation. The CLI edits YAML, not Nickel source expressions. The evaluator and library are embedded; evaluation never downloads a library or executes hooks.

## Installation and prerequisites

Runtime prerequisites: Linux, Docker Engine, and Docker Compose >= 2.24. Swarm deployment additionally requires a manager context and a reachable registry for built images. Docker installation, Swarm initialization, registry infrastructure, and networking are explicit administrator prerequisites; Dockstride never installs packages or changes them silently.

Install the latest release with curl:

```sh
curl -fsSL https://raw.githubusercontent.com/MatthewScholefield/dockstride/main/scripts/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
dks --version
```

The installer detects Linux x86_64 or ARM64, verifies the archive's SHA-256 checksum and binary version, and atomically installs `dks` to `~/.local/bin`. No sudo, Rust toolchain, Docker installation, or host package changes. A failed download or verification leaves an existing `dks` unchanged. Add `~/.local/bin` to your shell's persistent `PATH` if needed.

To inspect before executing, or pin a version/custom destination:

```sh
curl -fsSL https://raw.githubusercontent.com/MatthewScholefield/dockstride/main/scripts/install.sh -o install-dockstride.sh
less install-dockstride.sh
DOCKSTRIDE_VERSION=v0.1.0 DOCKSTRIDE_INSTALL_DIR="$HOME/bin" sh install-dockstride.sh
```

[GitHub Releases](https://github.com/MatthewScholefield/dockstride/releases) also provides archives and checksum files for manual installation. Checksums detect corruption; publisher authenticity relies on HTTPS and the GitHub repository/release.

```sh
# Source installation on the current Linux host
cargo install --path . --locked

# Static release, with an explicitly selected isolated Alpine compiler
scripts/release.sh --container --verify-reproducible
```

Every push to `main` and every pull request automatically tests and builds native Linux x86_64/ARM64 binaries in [GitHub Actions](https://github.com/MatthewScholefield/dockstride/actions/workflows/release.yml). Pushing a version tag such as `v0.1.0` publishes both verified archives and checksums after both builds pass. Branch builds are available as Actions artifacts; curl installs the latest published stable release, not an untagged branch build.

The release workflow uses native runners; the script rejects accidental cross-target builds. Docker remains external to the static executable. Archives include `dks`, the pinned library, a runnable starter, provenance, and checksums. [Release details](docs/reference.md#release-and-verification).

## From checkout to development

```sh
dks init
dks dev --trust
```

`init` refuses to overwrite existing application files. `dev` fills missing ordinary values, provisions secrets once, validates ownership and ports, builds/starts the declared stack, waits for actual readiness, reports endpoints, and enters declared Compose watch or `dockstride.dev.argv`. `up` performs startup without the foreground development loop.

```sh
dks setup                         # Configure/provision without starting containers
dks config list
dks config set oauth.enabled true
dks config unset apiPort           # Restore the declared default
dks render
dks doctor
dks up --plan                     # Inspection only; unresolved inputs stay unresolved
dks up --trust
dks status
dks logs -f api
dks exec api sh
dks down                          # Preserve volumes, secrets, allocated endpoints
dks destroy --plan
dks destroy --yes                 # Delete only owned application data, not secrets
```

`--trust` explicitly authorizes Dockerfiles, project argv hooks, and privileged Docker configuration. Without it, noninteractive execution fails before setup. Inspection runs no project shell commands, but Nickel imports can read accessible files: rendering an untrusted project is **not** a security sandbox.

Ctrl-C stops foreground work; it does not undo completed setup or deployment and does not stop detached containers. Stop an active foreground development loop before another lifecycle mutation. `down` stops owned containers. Running without healthchecks is reported as running, not proven healthy. Hot reload exists only when declared by the project.

The richer [sample application](examples/sample/compose.ncl) includes UID-1000 secret consumers, one-shot migrations, persistent data, HTTP identity checks, automatic checkout-local port allocation, content watch, and a production worker. Its file permission policy must match the invoking host UID/GID; see the reference before running it.

`setup` provisions inputs and reserves explicitly declared checkout-local ports without starting containers. Automatic port allocation is only for local Unix-socket contexts; remote environments use explicit ports.

## Automation and secret inputs

```sh
dks setup --non-interactive --set project=sample-ci
# Initial prompt-based secrets also have file/stdin equivalents:
dks setup --non-interactive --secret-file authKey=/private/auth-key
dks setup --non-interactive --secret-stdin authKey < /private/auth-key
dks up --non-interactive --trust --json
```

Initial input flags affect only missing references. Existing credentials are reused; changing credentials requires explicit replacement. Secret values are never command-line arguments. `--json` emits version-1 newline-delimited events and a terminal result/error; subprocess output cannot corrupt stdout. Missing ordinary values are returned as structured inputs instead of prompting. Human native passthrough preserves raw output and interactive terminal input. `--no-color`, `NO_COLOR`, help, completions, and `--` forwarding are supported.

## Secret lifecycle

```sh
dks secrets list
dks secrets replace authKey
dks secrets replace authKey --file /private/new-key
dks secrets replace authKey --stdin < /private/new-key
dks secrets gc --plan
dks secrets gc EXACT_REVISION_FROM_PLAN --yes
```

Private files support local development; immutable Swarm objects support production. Applications consume `/run/secrets/<logical-name>` and must implement their own `_FILE` handling. References, revisions, and consumers are visible; bytes are not.

Missing recorded secrets fail with recovery guidance—never automatic regeneration. Replacement retains the previous revision and does not restart applications. `--apply --trust` requires a declared application-specific rotation procedure; storage replacement is not a universal database password/encryption-key rotation.

The default file store is a marked user-private directory under `XDG_DATA_HOME` (normally `~/.local/share/dockstride/secrets`) with non-listable per-user mode-0300 directories. An optional root-owned shared parent is provisioned only by the explicit executable [administration script](scripts/setup-secret-storage.sh). See [permissions, rootless contexts, and recovery](docs/reference.md#secrets).

## From the same definition to production

In a **separate checkout** configured with `backend: swarm`, an explicit registry image prefix, and durable secret recovery sources:

```sh
dks setup
dks deploy --plan
dks deploy --trust
dks deploy api --trust             # Only the selected service and explicit prerequisites
dks status
dks logs -f api
```

Built images use immutable revisions and digest references. Repeated identical builds reuse the published revision. Full deployment does not prune omitted services. Selected deployment uses direct service operations, does not deploy dependencies implicitly, and rejects incompatible/shared changes before applying them. Rollout failures return failure with task diagnostics; snapshots and owned live specifications support interrupted-operation recovery.

There is no universal zero-downtime guarantee or transactional rollback across services, migrations, and database state. Old secrets and snapshots remain available deliberately. Multi-node registries must be reachable by every node; loopback-only registry prefixes are rejected on multi-node clusters. Destructive multi-node volume removal is rejected rather than guessing which node owns data.

## Inspection and escape hatches

```sh
dks config schema --json
dks render --target compose
dks render --target swarm
dks render --target build
dks compose logs -f api
dks stack services
dks completions bash
```

Rendering after identity establishment includes the same managed ownership labels used for execution. Before identity exists, rendering is canonical and does not allocate an identity. Plans disclose Docker operations, images, prerequisites, secrets, shared resources, and unresolved allocations without generating secrets, editing YAML, building, or deploying. Native namespaces retain Docker semantics; unknown commands never become passthrough.

## Architecture and verification

Small Rust modules separate Nickel, document-aware configuration, canonical Docker views, planning/execution, secrets, state, and presentation. Connections are pinned per operation; context switching cannot redirect later managed commands. Ownership is a random persisted token, never an inferred folder basename.

The pinned Nickel 0.19.0 evaluator contains a minimal, regression-tested repair for nested deserialized-data merges. [Patch provenance and compatibility](vendor/nickel-lang-core/PATCHES.md) explain the invariant and why unpatched published 0.19.0 is not a compatible standalone exporter for these fixtures. No assertions are suppressed and no panic fallback is used.

```sh
cargo test --locked --all-targets
scripts/smoke.py --dks target/debug/dks --compose
scripts/smoke.py --dks target/debug/dks --swarm
```

The Swarm smoke uses disposable manager/worker/registry containers and separate contexts; it never initializes the selected existing daemon. It requires privileged Docker-in-Docker and nested Docker networking. Its explicit `swarmDirectNetworking=true` profile verifies host-published/DNS-round-robin services, two-node image/secret distribution, and selected updates without IPVS. Ingress routing mesh is not verified on this rootless host. Unsupported infrastructure is an explicit verification failure, not a silently skipped success. The smoke cleans up its own UUID-named resources.

[Complete configuration/lifecycle/automation reference](docs/reference.md). [Changelog](CHANGELOG.md).
