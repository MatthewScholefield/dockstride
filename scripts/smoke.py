#!/usr/bin/env python3
"""Real Dockstride consumer smoke. No Docker resources are used without a flag.

  scripts/smoke.py --dks target/debug/dks
  scripts/smoke.py --dks target/debug/dks --compose
  scripts/smoke.py --dks target/debug/dks --swarm

Compose uses only UUID-named project resources on the selected Docker context.
Swarm NEVER initializes that daemon: it creates disposable Docker-in-Docker
manager/worker containers and a private registry/network, then a separate context.
The host must support privileged DIND containers, bridge networking, and overlay
networks. Fixture gateways permit inter-container communication and use Docker's
userland proxy; isolation is the UUID-scoped outer network and disposable daemons.
The sample's explicit direct-networking profile uses host-mode API publication
and DNS round-robin, not the IPVS ingress routing mesh (unavailable in rootless
user namespaces). Failures are not skipped or reported as successful verification.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid


class SmokeFailure(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise SmokeFailure(message)


def run(argv, env=None, timeout=300, ok=True):
    result = subprocess.run([str(x) for x in argv], env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    if ok and result.returncode:
        raise SmokeFailure(f"command failed ({result.returncode}): {' '.join(map(str, argv))}\n"
                           f"{result.stdout}\n{result.stderr}")
    return result


def reserve_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def poll(action, predicate, description, timeout=90):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = action()
            if predicate(last):
                return last
        except (OSError, ValueError, urllib.error.URLError) as error:
            last = str(error)
        time.sleep(0.5)
    raise SmokeFailure(f"timed out waiting for {description}: {last}")


class Harness:
    def __init__(self, args, root):
        self.args = args
        self.root = root
        self.prefix = "dks-smoke-" + uuid.uuid4().hex[:12]
        self.binary = str(Path(args.dks).resolve())
        require(Path(self.binary).is_file(), f"build dks first; binary missing: {self.binary}")
        self.env = os.environ.copy()
        # Keep Docker's selected context/credentials, but isolate both standard
        # storage roots so even a HOME-fallback regression cannot touch host data.
        if not self.env.get("DOCKER_CONFIG"):
            self.env["DOCKER_CONFIG"] = str(Path.home() / ".docker")
        self.env.update(NO_COLOR="1", HOME=str(root / "home"),
                        XDG_DATA_HOME=str(root / "private"), XDG_STATE_HOME=str(root / "state"))
        self.host_env = self.env.copy()
        self.projects = []
        self.containers = []
        self.contexts = []
        self.network = None
        self.watch = None
        self.report = []
        self.git_fixture()

    def git_fixture(self):
        repo = self.root / "repository"
        shutil.copytree(Path(__file__).resolve().parents[1] / "examples" / "sample", repo)
        run(["git", "init", "--quiet", repo])
        run(["git", "-C", repo, "add", "."])
        run(["git", "-C", repo, "-c", "user.name=Dockstride smoke", "-c",
             "user.email=smoke@invalid.example", "commit", "--quiet", "-m", "Disposable sample fixture"])
        self.repo = repo

    def checkout(self, suffix):
        path = self.root / suffix
        run(["git", "-C", self.repo, "worktree", "add", "--quiet", "--detach", path])
        return path

    def cli(self, project, *args, fail=False, timeout=300, env=None):
        result = run([self.binary, "-C", project, "--json", "--non-interactive", "--no-color",
                      "--timeout", str(self.args.timeout), *args], env or self.env, timeout, ok=False)
        records = []
        for line in result.stdout.splitlines():
            try:
                record = json.loads(line)
            except ValueError as error:
                raise SmokeFailure(f"CLI corrupted JSON stdout: {line!r}\n{result.stderr}") from error
            require(record.get("schemaVersion") == 1, "unknown result schema version")
            records.append(record)
        require(records, f"CLI produced no structured result: {result.stderr}")
        terminal = records[-1]
        if fail:
            require(result.returncode != 0 and terminal.get("type") == "error",
                    f"expected command failure: {args}: {result.stdout}\n{result.stderr}")
            require(terminal.get("exitCode") == result.returncode, "error exit code disagrees with process")
            return terminal
        require(result.returncode == 0 and terminal.get("type") == "result",
                f"CLI failed: {args}: {result.stdout}\n{result.stderr}")
        return terminal["result"]

    def docker(self, *args, env=None, ok=True, timeout=300):
        return run(["docker", *args], env or self.env, timeout, ok)

    def json_docker(self, *args, env=None):
        return json.loads(self.docker(*args, env=env).stdout)

    def config(self, project, field, value):
        encoded = json.dumps(value)
        self.cli(project, "config", "set", field, encoded)

    def setup(self, project, suffix, backend="compose", port=None):
        name = self.prefix + "-" + suffix
        self.projects.append((project, name, self.env.copy()))
        inputs = {"project": name, "backend": backend, "hostSecretUid": os.getuid(),
                  "hostSecretGid": os.getgid()}
        if port is not None:
            inputs["apiPort"] = port
        if backend == "swarm":
            recovery = self.root / "recovery"
            recovery.mkdir(mode=0o700, exist_ok=True)
            inputs.update(imagePrefix="registry:5000/" + name, containerGid=1000,
                          swarmDirectNetworking=True,
                          recoveryFile=str(recovery / (suffix + "-{revision}")))
        else:
            security = self.json_docker("info", "--format", "{{json .SecurityOptions}}")
            # Rootless GID zero maps to invoking host GID; native daemon needs host GID directly.
            inputs["containerGid"] = 0 if any("rootless" in x for x in security) else os.getgid()
        argv = ["setup"]
        for field, value in inputs.items():
            argv.extend(["--set", field + "=" + json.dumps(value)])
        self.cli(project, *argv)
        return name

    def identity(self, port, name, revision=None):
        def fetch():
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as response:
                return json.load(response)
        result = poll(fetch, lambda item: item.get("project") == name and
                      (revision is None or item.get("revision") == revision), "HTTP application identity")
        require(result["application"] == "dockstride-sample", "wrong application responded")
        require(result["uid"] == 1000, "secret consumer did not run as explicit non-root UID1000")
        require(len(result["secretFingerprint"]) == 64, "app did not verify secret access")
        return result

    def configured_port(self, project):
        return self.cli(project, "config", "get", "apiPort")["value"]

    def secret_ref(self, project):
        return self.cli(project, "render", "--target", "compose")["secrets"]["authKey"]

    def config_smoke(self):
        project = self.checkout("config")
        fields = self.cli(project, "config", "schema")["fields"]
        require(any(field["path"] == "oauth.enabled" and field["default"] is False for field in fields),
                "nested schema/default discovery failed without env.yaml")
        require(any(field["path"] == "project" and field["required"] for field in fields),
                "required configuration discovery failed")
        require(not (project / "env.yaml").exists(), "schema inspection created environment")
        plan = self.cli(project, "up", "--plan")
        require(plan.get("sideEffects") is False, "unresolved plan not side-effect free")
        require(not (project / "env.yaml").exists(), "plan created environment")
        self.config(project, "apiPort", 8123)
        self.config(project, "project", self.prefix + "-config")
        path = project / "env.yaml"
        path.write_text("# smoke comment\n" + path.read_text() + "# smoke trailing comment\n")
        self.config(project, "oauth.enabled", True)
        self.config(project, "apiPort", 8124)
        before = path.read_bytes()
        require(b"# smoke comment" in before and b"# smoke trailing comment" in before,
                "scalar edit lost YAML comments")
        self.cli(project, "config", "set", "apiPort", "70000", fail=True)
        require(path.read_bytes() == before, "invalid candidate overwrote environment")
        self.report.append("fresh checkout schema, incremental config, comments, invalid candidates, no-effect plan")

    def compose_smoke(self):
        first, second = self.checkout("compose-a"), self.checkout("compose-b")
        name_a = self.setup(first, "a")
        name_b = self.setup(second, "b")
        port_a, port_b = self.configured_port(first), self.configured_port(second)
        require(port_a != port_b, "worktrees allocated the same port")
        refs = (self.secret_ref(first), self.secret_ref(second))
        require(refs[0] != refs[1], "worktrees share generated credentials")
        before_plan = (first / "env.yaml").read_bytes()
        self.cli(first, "up", "--plan")
        require((first / "env.yaml").read_bytes() == before_plan and self.secret_ref(first) == refs[0],
                "resolved plan mutated configuration or secret reference")
        blocked = self.checkout("compose-blocked")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            occupied = listener.getsockname()[1]
            self.setup(blocked, "blocked", port=occupied)
            error = self.cli(blocked, "up", fail=True, timeout=900)
            require("port" in error["message"].lower() or "address already in use" in error["message"].lower(),
                    "occupied-port failure did not explain the conflict")
            require(listener.fileno() >= 0 and listener.getsockname()[1] == occupied,
                    "startup commandeered unrelated listener")
        for project in (first, second):
            self.cli(project, "up", timeout=900)
        initial = self.identity(port_a, name_a)
        other = self.identity(port_b, name_b)
        require(initial["migrated"] and other["migrated"], "one-shot migration did not complete")
        require(initial["secretFingerprint"] != other["secretFingerprint"], "worktree secrets are not isolated")
        self.cli(first, "status")
        self.cli(first, "logs", "--tail", "20", "api")
        self.cli(first, "exec", "api", "--", "python", "-c",
                 "import os,pathlib; assert os.getuid()==1000; assert pathlib.Path('/run/secrets/authKey').read_bytes()")
        volumes = self.docker("volume", "ls", "--quiet", "--filter", "label=com.docker.compose.project=" + name_a).stdout.split()
        require(volumes, "sample did not create persistent application data")
        self.cli(first, "down")
        for volume in volumes:
            self.docker("volume", "inspect", volume)
        require(self.secret_ref(first) == refs[0], "down changed secret reference")
        self.cli(first, "up", timeout=900)
        require(self.identity(port_a, name_a)["secretFingerprint"] == initial["secretFingerprint"],
                "restart regenerated credential")
        require(self.configured_port(first) == port_a, "restart moved endpoint")
        self.watch_smoke(first, port_a, name_a)
        self.cli(first, "destroy", "--plan")
        self.cli(first, "destroy", "--yes")
        for volume in volumes:
            require(self.docker("volume", "inspect", volume, ok=False).returncode != 0,
                    "destroy preserved application volume")
        require(self.secret_ref(first) == refs[0], "destroy removed secret reference")
        require(self.identity(port_b, name_b)["secretFingerprint"] == other["secretFingerprint"],
                "destroy affected unrelated worktree")
        self.config(first, "failMigration", True)
        error = self.cli(first, "up", fail=True, timeout=900)
        require("migrat" in error["message"].lower(),
                f"migration failure diagnostic omitted service: {json.dumps(error)}")
        logs = self.docker("logs", self.compose_container(name_a, "migrate"))
        require("sample migration failed" in logs.stdout + logs.stderr or "sample migration failed" in error["message"],
                f"migration error was not readable: {json.dumps(error)}")
        self.config(first, "failMigration", False)
        self.config(first, "failHealth", True)
        error = self.cli(first, "up", fail=True, timeout=900)
        require(any(word in error["message"].lower() for word in ("health", "readiness", "api")),
                f"readiness failure lacked application diagnostic: {json.dumps(error)}")
        self.report.append("Compose image build/HTTP identity, non-root secrets, migrations/watch, isolated worktrees/ports, restart, status/logs/exec/down/destroy, failure diagnostics")

    def compose_container(self, name, service):
        items = self.docker("ps", "-aq", "--filter", "label=com.docker.compose.project=" + name,
                            "--filter", "label=com.docker.compose.service=" + service).stdout.split()
        require(len(items) == 1, f"expected one {service} container: {items}")
        return items[0]

    def watch_smoke(self, project, port, name):
        log = self.root / "watch.log"
        with log.open("w+") as output:
            self.watch = subprocess.Popen([self.binary, "-C", str(project), "--non-interactive",
                                           "--timeout", str(self.args.timeout), "dev"], env=self.env,
                                          stdout=output, stderr=output, start_new_session=True)
            try:
                poll(lambda: log.read_text(), lambda text: "watch enabled" in text.lower(),
                     "Compose watch ready")
                revision = "watched-" + uuid.uuid4().hex[:8]
                (project / "api" / "content" / "revision.txt").write_text(revision + "\n")
                self.identity(port, name, revision)
                self.watch.send_signal(signal.SIGINT)
                self.watch.wait(timeout=20)
                require(self.watch.returncode in (0, 130), f"unexpected watch cancellation: {log.read_text()}")
                self.identity(port, name, revision)
            except SmokeFailure as error:
                raise SmokeFailure(f"{error}\nCompose watch output:\n{log.read_text()}") from error
            finally:
                if self.watch.poll() is None:
                    os.killpg(self.watch.pid, signal.SIGTERM)
                    self.watch.wait(timeout=20)
                self.watch = None

    def dind_fixture(self):
        self.network = self.prefix + "-network"
        self.docker("network", "create", "--label", "dockstride.smoke=" + self.prefix,
                    self.network, env=self.host_env)
        registry = self.prefix + "-registry"
        self.containers.append(registry)
        self.docker("run", "-d", "--name", registry, "--network", self.network,
                    "--network-alias", "registry", "--label", "dockstride.smoke=" + self.prefix,
                    self.args.registry_image, env=self.host_env)
        self.app_port = reserve_port()
        self.node_envs = {}
        for role in ("manager", "worker"):
            name = self.prefix + "-" + role
            api = reserve_port()
            self.containers.append(name)
            argv = ["run", "-d", "--privileged", "--name", name, "--network", self.network,
                    "--label", "dockstride.smoke=" + self.prefix, "-e", "DOCKER_TLS_CERTDIR=",
                    "-p", f"127.0.0.1:{api}:2375"]
            if role == "manager":
                argv += ["-p", f"127.0.0.1:{self.app_port}:{self.app_port}"]
            argv += [self.args.dind_image, "--host=tcp://0.0.0.0:2375", "--host=unix:///var/run/docker.sock",
                     "--insecure-registry=registry:5000", "--userland-proxy=true"]
            self.docker(*argv, env=self.host_env)
            context = self.prefix + "-" + role
            self.contexts.append(context)
            self.docker("context", "create", context, "--docker", f"host=tcp://127.0.0.1:{api}", env=self.host_env)
            environment = self.host_env.copy()
            environment.pop("DOCKER_HOST", None)
            environment.pop("DOCKER_TLS_VERIFY", None)
            environment.pop("DOCKER_CERT_PATH", None)
            environment["DOCKER_CONTEXT"] = context
            self.node_envs[role] = environment
            try:
                poll(lambda: self.docker("info", env=environment, ok=False, timeout=10),
                     lambda result: result.returncode == 0, role + " disposable DIND daemon", timeout=120)
            except SmokeFailure as error:
                logs = self.docker("logs", name, env=self.host_env, ok=False).stdout
                raise SmokeFailure(f"host cannot start isolated DIND {role}; Swarm was NOT verified.\n{logs}\n{error}") from error
            # Swarm's automatic gateway disables ICC, which requires br_netfilter.
            # A fixture has no mutually untrusted tenants: the outer UUID network
            # and disposable daemon/container boundary provide isolation instead.
            # Pre-create the documented gateway on BOTH nodes before init/join;
            # ICC plus the userland proxy needs no bridge-netfilter suppression.
            self.docker("network", "create", "--driver", "bridge",
                        "--opt", "com.docker.network.bridge.name=docker_gwbridge",
                        "--opt", "com.docker.network.bridge.enable_icc=true",
                        "--opt", "com.docker.network.bridge.enable_ip_masquerade=true",
                        "docker_gwbridge", env=environment)
        self.env = self.node_envs["manager"]
        address = self.docker("inspect", "--format", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
                              self.prefix + "-manager", env=self.host_env).stdout.strip()
        require(address, "disposable manager has no private network address")
        self.docker("swarm", "init", "--advertise-addr", address)
        self.manager_address = address

    def swarm_smoke(self):
        self.dind_fixture()
        project = self.checkout("swarm")
        name = self.setup(project, "swarm", "swarm", self.app_port)
        secret_before = self.secret_ref(project)
        before_plan = (project / "env.yaml").read_bytes()
        self.cli(project, "deploy", "--plan")
        require((project / "env.yaml").read_bytes() == before_plan and self.secret_ref(project) == secret_before,
                "Swarm plan mutated environment or secret")
        self.cli(project, "deploy", timeout=900)
        identity = self.identity(self.app_port, name)
        # First prove an actual single-node cluster before joining the second node.
        require(len(self.docker("node", "ls", "-q").stdout.split()) == 1, "fixture was not single-node")
        self.cli(project, "deploy", timeout=900)
        require(self.secret_ref(project) == secret_before, "repeated deployment regenerated Swarm secret")
        require(self.identity(self.app_port, name)["secretFingerprint"] == identity["secretFingerprint"],
                "repeated deployment changed application credential")
        token = self.docker("swarm", "join-token", "-q", "worker").stdout.strip()
        self.docker("swarm", "join", "--token", token, self.manager_address + ":2377", env=self.node_envs["worker"])
        poll(lambda: self.docker("node", "ls", "--format", "{{.Status}} {{.Availability}}").stdout,
             lambda text: text.count("Ready Active") == 2, "two-node disposable Swarm")
        self.config(project, "workerOnSeparateNode", True)
        self.cli(project, "deploy", timeout=900)
        worker_spec = self.json_docker("service", "inspect", name + "_worker")[0]
        require("@sha256:" in worker_spec["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"],
                "Swarm build did not deploy immutable digest")
        worker_id = poll(lambda: self.docker("ps", "-q", "--filter", "label=com.docker.swarm.service.name=" + name + "_worker",
                                             env=self.node_envs["worker"]).stdout.strip(),
                         bool, "worker-node application task")
        proof = self.docker("exec", worker_id, "python", "-c",
                            "import json,urllib.request; print(json.dumps(json.load(urllib.request.urlopen('http://127.0.0.1:8000/health'))))",
                            env=self.node_envs["worker"])
        worker_identity = json.loads(proof.stdout)
        require(worker_identity["project"] == name and worker_identity["uid"] == 1000 and
                worker_identity["secretFingerprint"] == identity["secretFingerprint"],
                "worker did not pull/read the same image/secret from shared registry and Swarm")
        require(self.secret_ref(project) == secret_before, "two-node deployment regenerated secret")
        revision = "selected-" + uuid.uuid4().hex[:8]
        (project / "api" / "content" / "revision.txt").write_text(revision + "\n")
        self.cli(project, "deploy", "api", timeout=900)
        selected_identity = self.identity(self.app_port, name, revision)
        require(self.secret_ref(project) == secret_before and
                selected_identity["secretFingerprint"] == identity["secretFingerprint"],
                "selected deployment regenerated application credential")
        after = self.json_docker("service", "inspect", name + "_worker")[0]
        require(after["Version"] == worker_spec["Version"] and after["Spec"] == worker_spec["Spec"],
                "selected API deploy modified unrelated worker service")
        self.cli(project, "status")
        self.cli(project, "logs", "--tail", "20", "api")
        self.config(project, "failHealth", True)
        error = self.cli(project, "deploy", "api", fail=True, timeout=900)
        require(any(word in error["message"].lower() for word in ("rollout", "task", "health", "converge", "pause")),
                "failed rollout lacks actionable task/health/convergence error")
        self.cli(project, "down")
        require(self.secret_ref(project) == secret_before, "Swarm down changed secret reference")
        self.report.append("disposable single-node and two-node Swarm (host publication/DNS round-robin, not ingress routing mesh): published digests distributed to worker, full/repeated/selected deploy, secret reuse, status/logs, failed rollout, teardown")

    def cleanup(self):
        errors = []
        if self.watch and self.watch.poll() is None:
            os.killpg(self.watch.pid, signal.SIGTERM)
            self.watch.wait(timeout=20)
        for project, name, environment in reversed(self.projects):
            try:
                # Each name is a freshly generated fixture UUID, never a user project.
                run([self.binary, "-C", project, "--non-interactive", "destroy", "--yes"],
                    environment, timeout=90, ok=False)
                if environment.get("DOCKER_CONTEXT") in self.contexts:
                    self.docker("stack", "rm", name, env=environment, ok=False)
                else:
                    for kind, label in (("container", "com.docker.compose.project"),
                                        ("network", "com.docker.compose.project"),
                                        ("volume", "com.docker.compose.project")):
                        verb = ["ps", "-aq"] if kind == "container" else [kind, "ls", "-q"]
                        items = self.docker(*verb, "--filter", "label=" + label + "=" + name,
                                            env=environment, ok=False).stdout.split()
                        for item in items:
                            removal = ["rm", "-f", "-v", item] if kind == "container" else [kind, "rm", item]
                            result = self.docker(*removal, env=environment, ok=False)
                            if result.returncode:
                                errors.append(result.stderr)
            except (OSError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append(str(error))
        for name in reversed(self.containers):
            result = self.docker("rm", "-f", "-v", name, env=self.host_env, ok=False)
            if result.returncode and "No such container" not in result.stderr:
                errors.append(result.stderr)
        if self.network:
            result = self.docker("network", "rm", self.network, env=self.host_env, ok=False)
            if result.returncode and "not found" not in result.stderr:
                errors.append(result.stderr)
        for name in reversed(self.contexts):
            result = self.docker("context", "rm", "-f", name, env=self.host_env, ok=False)
            if result.returncode:
                errors.append(result.stderr)
        # Private secret directories deliberately prohibit listing; restore owner
        # traversal only inside this disposable fixture for complete local cleanup.
        private = self.root / "private"
        if private.exists():
            for directory, children, _ in os.walk(private):
                for child in children:
                    path = Path(directory) / child
                    if not path.is_symlink():
                        try:
                            path.chmod(0o700)
                        except PermissionError:
                            pass
        if errors:
            raise SmokeFailure("fixture cleanup failed:\n" + "\n".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dks", default="target/debug/dks", help="already-built CLI; this harness never builds Rust")
    parser.add_argument("--compose", action="store_true", help="exercise Compose on the selected daemon with UUID-owned resources")
    parser.add_argument("--swarm", action="store_true", help="create isolated DIND manager+worker+registry; NEVER initialize existing daemon")
    parser.add_argument("--timeout", type=int, default=90, help="Dockstride readiness/rollout timeout in seconds")
    parser.add_argument("--dind-image", default="docker:dind")
    parser.add_argument("--registry-image", default="registry:2")
    args = parser.parse_args()
    harness = None
    failure = None
    report = []
    with tempfile.TemporaryDirectory(prefix="dockstride-smoke-") as temporary:
        try:
            harness = Harness(args, Path(temporary))
            harness.config_smoke()
            if args.compose:
                harness.compose_smoke()
            if args.swarm:
                harness.swarm_smoke()
            report = harness.report
        except (SmokeFailure, OSError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            failure = str(error)
        finally:
            if harness:
                try:
                    harness.cleanup()
                except (SmokeFailure, OSError, subprocess.TimeoutExpired) as error:
                    failure = (failure + "\n" if failure else "") + str(error)
    if failure:
        print("SMOKE FAILED: " + failure, file=sys.stderr)
        return 1
    for check in report:
        print("PASS " + check)
    if not args.compose:
        print("NOT EXERCISED Compose (enable --compose)")
    if not args.swarm:
        print("NOT EXERCISED Swarm / multi-node image distribution (enable --swarm)")
    print("PASS disposable resource cleanup")
    return 0


if __name__ == "__main__":
    sys.exit(main())
