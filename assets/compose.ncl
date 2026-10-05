# The bundled library keeps backend conversion and validation local and inspectable.
let lib = import "libs/dockstride.ncl" in
# Declare editable env.yaml values here; dks setup asks for required values.
let configContract = {
  project | String | doc "Unique lowercase Compose project or Swarm stack name.",
  backend | lib.Backend | doc "Runtime backend." | default = "compose",
  apiPort | lib.Port | doc "Host HTTP port." | default = 8080,
} in
# Environment values are separate from the checked-in architecture.
let env | configContract = import "env.yaml" in
let dc = lib.forEnvironment env in
# ComposeFile renders the same definition for Compose or Swarm.
dc.ComposeFile {
  # Dockstride metadata is available to the CLI but not exported to Docker.
  dockstride | not_exported = {
    Config = configContract,
    # Optional trusted JSON discovery: commands.devDefaults = { argv = ["python3", "scripts/dev-defaults.py"] },
    # setup.defaults = { command = "devDefaults", fields = ["project"], sources = true },
    endpoints.hello = "http://localhost:%{env.apiPort}",
  },
  # Replace this small prebuilt service with your application's services.
  services.hello = dc.Service {
    image = "hashicorp/http-echo:1.0.0",
    command = ["-listen=:5678", "-text=Hello world!"],
    ports = ["%{env.apiPort}:5678"],
  },
}
