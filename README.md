# Dockstride

*Your whole devloop and deploy setup in a single executable*

Deploy and develop your code with ease (yes, I, a human, wrote that 😂). Dockstride is a simple set of systems that work well together to deploy your code end to end from a single Docker Compose config.

**So how does it work?**

- **Define your services:** [Docker Compose](https://docs.docker.com/compose/) + [Nickel](https://github.com/nickel-lang/nickel)
  - Use Dockstride's Nickel library for improved defaults and simple helpers
  - Define a `Config` type for your service's env-specific parameters
- **Deploy your services:**
  - **Development:** `dks up` to start your services with Docker Compose, or `dks dev` for file watching and your devloop
  - **Production:** `dks deploy` to deploy to [Docker Swarm](https://docs.docker.com/engine/swarm/)

That's it! In addition, Dockstride includes a lot of other useful features like:
- Automatic startup monitoring
- Secret provisioning and management
- Building and publishing images for production
- Deploying individual services

## Getting Started

### Install

You'll need Linux, Docker Engine, and Docker Compose 2.24 or newer.

```sh
curl -fsSL https://raw.githubusercontent.com/MatthewScholefield/dockstride/main/scripts/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
```

### Define your services

In your project directory:

```sh
dks init
```

This creates `compose.ncl`, `libs/dockstride.ncl`, and `env.yaml`. Here's the starter `compose.ncl`, without the comments:

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

Replace the `hello` service with your own services when you're ready.

### Set up your env

```sh
dks setup
```

This asks for missing settings and saves them to `env.yaml`. For this example, just enter `hello` as the project name:

```yaml
project: hello
```

The backend defaults to Compose and the port to 8080. You can change them in `env.yaml`, or use `dks config set apiPort 8081`. Invalid values are rejected.

Commit `compose.ncl` and the library, but not `env.yaml`. Each environment gets its own settings. (`dks init` should have gitignored the file for you)

### Deploy your services

Start locally:

```sh
dks up --trust
curl http://localhost:8080
```

You should get `Hello world!`. If you changed the port, use that instead.

Use `dks dev --trust` for a devloop once you've added Compose watch rules or a development command. The starter doesn't have either. `--trust` allows Dockstride to run the project's builds and commands, so only use it on code you trust.

To stop your services:

```sh
dks down
```

#### Production

On your production server, install Docker and `dks`, then get a separate checkout of your project.

Swarm comes with Docker. For a single-server deployment, enable it with:

```sh
docker swarm init
```

Then, in your project directory:

```sh
dks setup --non-interactive --set project=hello-prod --set backend=swarm
dks deploy --trust
```

Your service is now available on port 8080 of the server. Make sure the firewall allows it.

For services with a `build`, you'll also need to configure a registry repository that your servers can reach. Dockstride builds and pushes the images for you. This example uses a public image, so it doesn't need that setup.

To deploy changes to just one service:

```sh
dks deploy hello --trust
```

### Finished!

For a bigger example with builds, secrets, persistent data, migrations, and file watching, see [the sample project](examples/sample/compose.ncl).

A few other useful commands:
- `dks logs -f hello` to follow logs
- `dks status` to check your services
- `dks render` to see the generated Docker config
- `dks deploy --plan` to see what a deployment would do
- `dks compose` and `dks stack` to use the underlying Docker commands

See the [reference](docs/reference.md) for the details.

## Alternatives

### Why not Kubernetes?

Kubernetes can look simple at first, but running it means dealing with a lot more than your application's services. There's the control plane, networking, resource definitions, and extra CPU and RAM usage. For a single server, you'll probably use something like K3s. That makes installation and resource usage smaller, but you still have Kubernetes to manage.

There are plenty of tools that try to make Kubernetes easier. They help with parts of it, but you often end up learning and maintaining those tools as well. If you need Kubernetes, use it. For deploying a few services on one or a few servers, Compose and Swarm are much simpler.

### Why not vanilla Compose? Why Nickel?

Flat env files get awkward as a project grows. You end up with a bunch of variables and defaults that have to line up for the app to work, and it's easy to miss a required value or pass an invalid one. Trying to keep things DRY with `-f compose.dev.yaml` and other overlays gives you another set of files to maintain.

With Nickel, you define your services once and give the env-specific settings types, defaults, and documentation. Each environment has a small `env.yaml` containing its values. Dockstride can ask you for what's missing and validate what you enter.

### Why not Jsonnet?

Jsonnet isn't bad, but it doesn't have a built-in type system. If your env lives in a separate file, catching typos and invalid values depends on validation you write yourself.

Dockstride also needs to know what settings exist and what types they accept to guide you through setup. Nickel gives us that information directly.
