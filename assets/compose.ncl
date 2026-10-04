let lib = import "libs/dockstride.ncl" in
let configContract = {
  project | String | doc "Unique lowercase Compose project or Swarm stack name.",
  backend | lib.Backend | doc "Runtime backend." | default = "compose",
  apiPort | lib.Port | doc "Host HTTP port." | default = 8080,
  imagePrefix | String | doc "Image repository prefix; use a reachable registry for Swarm." | default = "dockstride-sample",
  oauth = {
    enabled | Bool | default = false,
    issuer | String | default = "",
  },
  secrets = {
    authKey | lib.SecretSource | doc "Authentication signing key (secret reference, never plaintext).",
  },
} in
let env | configContract = import "env.yaml" in
let dc = lib.forEnvironment env in
dc.ComposeFile {
  dockstride | not_exported = {
    Config = configContract,
    setup.secrets.authKey = lib.GenerateSecret {bytes = 32, encoding = "hex"},
    endpoints.api = "http://localhost:%{env.apiPort}",
  },
  secrets = env.secrets,
  services.api = dc.Service {
    image = dc.image "api",
    build.context = "./app",
    # Explicit numeric identity makes the private 0600 development grant verifiable.
    # The richer sample demonstrates non-root consumers and mapped-group access.
    user = "0:0",
    ports = ["%{env.apiPort}:8000"],
    secrets = dc.grantSecrets ["authKey"],
    environment = dc.Env {
      AUTH_KEY_FILE = dc.secretPath "authKey",
      OAUTH_ENABLED = env.oauth.enabled,
      OAUTH_ISSUER = env.oauth.issuer,
    },
    healthcheck = {
      test = ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health', timeout=2)"],
      interval = "2s",
      timeout = "3s",
      retries = 15,
      start_period = "3s",
    },
    # Rebuild avoids archive-copy remounts of read-only secret binds on rootless Docker.
    develop.watch = [{action = "rebuild", path = "./app/server.py"}],
    deploy = {
      replicas = 1,
      update_config = {order = "start-first", failure_action = "rollback"},
      restart_policy.condition = "on-failure",
    },
  },
}
