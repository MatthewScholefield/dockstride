# Dockstride

*Your development loop and production deployment, managed by one executable from one typed Docker Compose definition.*

**How does it work?**

- **Define your services** with [Docker Compose](https://docs.docker.com/compose/) expressed in [Nickel](https://github.com/nickel-lang/nickel). Dockstride's library provides defaults and helpers; your `Config` contract defines the environment-specific settings.
- **Develop locally** with `dks up` or `dks dev`, backed by Docker Compose.
- **Deploy to production** with `dks deploy`, backed by [Docker Swarm](https://docs.docker.com/engine/swarm/). Use the same service definition with different environment values.

Docker runs your containers. Dockstride connects configuration, setup, startup, and deployment without requiring a separate configuration stack for each environment.

## Getting started

### Install

You'll need Linux, Docker Engine, and Docker Compose 2.24 or newer.

```sh
curl -fsSL https://raw.githubusercontent.com/MatthewScholefield/dockstride/main/scripts/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
```

The installer supports Linux x86_64 and ARM64. Nickel is embedded in `dks`; you don't need to install it separately. Docker remains a separate prerequisite.

### Define your services

In your project directory, run:

```sh
dks init
```

This creates `compose.ncl`, a pinned library at `libs/dockstride.ncl`, and an empty `env.yaml`. It also adds ignore rules for local configuration and Dockstride state.

Here's the generated definition, without the explanatory comments:

```nickel
let lib = import "libs/dockstride.ncl" in
let configContract = {
  project | String | doc "Unique lowercase Compose project or Swarm stack name.",
  backend | lib.Backend | doc "Runtime backend." | default = "compose",
  apiPort | lib.Port | doc "Host HTTP port." | default = 8080,
} in
let env | configContract = import "env.yaml" in
let dc = lib.forEnvironment env in

dc.ComposeFile {
  dockstride | not_exported = {
    Config = configContract,
    endpoints.hello = "http://localhost:%{env.apiPort}",
  },
  services.hello = dc.Service {
    image = "hashicorp/http-echo:1.0.0",
    command = ["-listen=:5678", "-text=Hello world!"],
    ports = ["%{env.apiPort}:5678"],
  },
}
```

It's one HTTP service, one required project name, and a configurable port. `Config` tells Dockstride which settings to ask for; Nickel supplies defaults and validates their types. The `dockstride` metadata isn't sent to Docker.

Replace `hello` with your application's services as you go. The [larger example](examples/sample/compose.ncl) shows builds, persistent data, secrets, migrations, and file watching.

### Set up your environment

```sh
dks setup
```

Dockstride asks for missing required settings, validates them, and writes `env.yaml`. Enter `hello` for the project name and you're ready:

```yaml
project: hello
```

The backend and port use their declared defaults. To override the port, either edit `env.yaml` or run:

```sh
dks config set apiPort 8081
```

Keep `compose.ncl` and the library in version control; keep `env.yaml` local to each environment. Setup also provisions secrets when your application declares them, storing references rather than secret bytes in YAML.

### Run locally

With the default port:

```sh
dks up --trust
curl http://localhost:8080
```

The response is `Hello world!`. If you changed `apiPort`, use that port instead.

`up` starts the services in the background. For applications with declared Compose watch rules or a development command, use `dks dev --trust` to start the foreground development loop. The hello-world example doesn't need one.

`--trust` authorizes execution of the project's Docker configuration, builds, and hooks. Only use it for projects you trust.

```sh
dks logs -f hello
dks status
dks down
```

`down` stops the local stack without deleting its data volumes.

### Deploy to production

Use a separate checkout on your production server so development and production have independent `env.yaml` files and state. Install Docker and `dks` there too.

For a single-server deployment, initialize Swarm on that server:

```sh
docker swarm init
```

A single manager is enough to run this example. To add servers later, Docker provides join commands for workers; networking and cluster administration remain yours to configure. Dockstride deploys to an existing manager rather than creating a cluster for you.

From the production checkout:

```sh
dks setup --non-interactive --set project=hello-prod --set backend=swarm
dks deploy --plan
dks deploy --trust
dks status
```

The same `compose.ncl` now runs as a Swarm stack. Visit port 8080 on your server, with that port allowed through its firewall.

This example uses a public image. For your own services with `build`, configure a registry repository reachable by your Swarm nodes; Dockstride builds and publishes immutable image revisions and deploys them by digest. See the [deployment reference](docs/reference.md#swarm-deployment-and-selected-scope) for the full workflow.

For subsequent changes, deploy the whole stack again or update just one service:

```sh
dks deploy hello --trust
```

That's the path: define once, configure each environment, run locally, then deploy.

## What else is included?

- **Startup monitoring:** wait for declared readiness checks and report failures. Without healthchecks, a running container isn't treated as proof of application health.
- **Development loops:** Compose file watching and explicit development commands.
- **Secret management:** provision secrets once, reuse their references, and replace them explicitly.
- **Inspectable operations:** `dks render` shows the Docker configuration; `--plan` previews startup or deployment without applying it.
- **Targeted deployments:** update a selected Swarm service without implicitly redeploying its dependencies.
- **Docker escape hatches:** `dks compose` and `dks stack` expose the underlying tools.

See the [reference](docs/reference.md) for configuration, lifecycle behavior, secrets, and automation.

## Why this approach?

### Why not Kubernetes?

Kubernetes makes sense when you need its ecosystem, scheduling capabilities, or organizational conventions. But for an application on one or a few servers, its control plane, resource model, networking, and supporting tools can be more infrastructure than the application needs.

Lightweight distributions such as K3s reduce CPU and memory overhead and make single-node Kubernetes practical; they don't remove the Kubernetes model you still have to operate. Helm charts, operators, and deployment platforms can help manage that complexity, but also introduce their own layers.

Dockstride takes a narrower approach: Compose for development, Swarm for deployment, and one service definition between them. It's not a replacement for every Kubernetes workload.

### Why not plain Docker Compose?

Plain Compose is a good starting point. The friction comes as configuration grows: flat environment variables, required values and defaults scattered across files, and environment-specific `-f compose.dev.yaml` overlays that must stay in sync. Invalid values often aren't discovered until startup.

Nickel lets you define a typed configuration contract alongside your services, with defaults, documentation, validation, and reusable functions. Each environment supplies values in `env.yaml` rather than another copy or overlay of the service definition. Dockstride uses that contract to guide setup and validate changes.

### Why Nickel rather than Jsonnet?

Jsonnet is useful for generating configuration and keeping it DRY. The difference here is an inspectable type contract: Jsonnet doesn't provide a built-in type system for a separate environment file. You can write assertions yourself, but a misspelled setting doesn't automatically become a schema error.

Nickel's contracts let Dockstride discover the expected inputs, explain them during setup, and reject invalid values. That makes the configuration language useful not just for generating Compose, but also for configuring the application interactively.
