let lib = import "libs/dockstride.ncl" in
let cfg = {
  project | String | doc "Unique identity for this checkout or deployment.",
  backend | lib.Backend | default = "compose",
  apiPort | lib.Port | doc "HTTP endpoint; setup allocates a checkout-local port." | default = 8080,
  imagePrefix | String | doc "Registry repository prefix for production builds." | default = "dockstride-sample",
  containerUid | Number | doc "Explicit non-root container user and private-secret owner." | default = 1000,
  failMigration | Bool | default = false,
  containerGid | Number | doc "Rootless Docker group zero maps to the invoking user's host group." | default = 0,
  hostSecretUid | Number | doc "Explicit host owner of private secret files; set to id -u." | default = 1000,
  hostSecretGid | Number | doc "Explicit authorized host group; set to id -g." | default = 1000,
  recoveryFile | String | doc "Private absolute Swarm backup path containing {revision}." | default = "",
  workerOnSeparateNode | Bool | doc "Constrain worker to a Swarm worker node when testing distribution." | default = false,
  swarmDirectNetworking | Bool | doc "Use host-mode API publication, DNS round-robin, and manager API placement instead of the IPVS routing mesh." | default = false,
  failHealth | Bool | default = false,
  oauth = {
    enabled | Bool | default = false,
    issuer | String | default = "",
  },
  secrets = { authKey | lib.SecretSource | doc "Authentication signing key; provisioned once, never stored inline." },
} in
let env | cfg = import "env.yaml" in
let dc = lib.forEnvironment env in
let app_uid = std.string.from env.containerUid in
let app_gid = std.string.from env.containerGid in
let port = std.string.from env.apiPort in
let health = {
  test = ["CMD-SHELL", "python -c \"import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health', timeout=1)\""],
  interval = "2s", timeout = "2s", start_period = "3s", retries = 10,
} in
let grant = [{ source = "authKey", target = "authKey", uid = app_uid, gid = app_gid, mode = 256 }] in
let service = fun role => dc.Service {
  image = dc.image role,
  build = "./api",
  user = "%{app_uid}:%{app_gid}",
  secrets = grant,
  healthcheck = health,
  environment = dc.Env {
    PROJECT = env.project,
    ROLE = role,
    AUTH_KEY_FILE = dc.secretPath "authKey",
    FAIL_HEALTH = if role == "api" then env.failHealth else false,
    REQUIRE_MIGRATION = if role == "api" && env.backend == "compose" then true else false,
  },
  deploy = {
    replicas = 1,
    restart_policy = { condition = "on-failure", max_attempts = 2 },
    update_config = { parallelism = 1, order = "stop-first", failure_action = "pause", monitor = "5s" },
  },
} & (if env.backend == "swarm" && env.swarmDirectNetworking then {
  deploy.endpoint_mode = "dnsrr",
} else {}) in
dc.ComposeFile {
  dockstride | not_exported = {
    Config = cfg,
    commands.devDefaults = { argv = ["python3", "dev-defaults.py"], timeoutSeconds = 30 },
    setup = {
      defaults = { command = "devDefaults", fields = ["project"], sources = true },
      secrets.authKey = lib.GenerateSecret { bytes = 32, encoding = "hex", durable = true, recoveryFile = env.recoveryFile },
      secretAccess.authKey = { uid = env.hostSecretUid, gid = env.hostSecretGid },
      ports.apiPort = { service = "api", target = 8000, from = 49152, to = 65535 },
    },
    endpoints.api = "http://127.0.0.1:%{port}",
    readiness.api = {
      url = "http://127.0.0.1:%{port}/health",
      status = 200,
      json = { application = "dockstride-sample", project = env.project, uid = env.containerUid },
    },
    actions = if env.backend == "compose" then [
      { name = "stop-consumers", kind = "stop", targets = ["api", "worker"],
        workflows = ["up", "dev"], stage = "before", services = ["api", "worker", "migrate"] },
      { name = "migrate", kind = "prerequisite", service = "migrate", fresh = true,
        workflows = ["up", "dev"], stage = "before", services = ["api", "worker", "migrate"] },
    ] else [],
    oneshots = if env.backend == "compose" then ["migrate"] else [],
  },
  secrets = env.secrets,
  volumes = { data = {} },
  services = {
    api = (service "api") & {
      volumes = ["data:/data"],
      ports = [if env.backend == "compose" then "127.0.0.1:%{port}:8000"
        else if env.swarmDirectNetworking then { target = 8000, published = env.apiPort, protocol = "tcp", mode = "host" }
        else "%{port}:8000"],
    } & (if env.backend == "compose" then {
      depends_on.migrate.condition = "service_completed_successfully",
      # Portable with read-only secrets, including rootless fuse-overlayfs daemons.
      develop.watch = [{ path = "./api/content", action = "rebuild" }],
    } else if env.swarmDirectNetworking then {
      deploy.placement.constraints = ["node.role == manager"],
    } else {}),
    worker = (service "worker") & (if env.backend == "swarm" && env.workerOnSeparateNode then {
      deploy.placement.constraints = ["node.role == worker"],
    } else {}),
  } & (if env.backend == "compose" then {
    migrate = dc.Service {
      image = dc.image "migrate",
      build = "./api",
      user = "%{app_uid}:%{app_gid}",
      command = ["python", "-u", "migrate.py"],
      environment = dc.Env { FAIL_MIGRATION = env.failMigration },
      volumes = ["data:/data"],
      restart = "no",
      healthcheck.disable = true,
    },
  } else {}),
}
