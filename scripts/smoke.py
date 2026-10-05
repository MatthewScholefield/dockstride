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
import stat
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
        self.outer_container_ids = {}
        self.node_envs = {}
        self.dind_nodes = {}
        self.swarm_identity = None
        self.host_daemon_id = None
        self.dind_data_paths = []
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

    def cli(self, project, *args, fail=False, timeout=300, env=None, lifecycle_timeout=None):
        budget = self.args.timeout if lifecycle_timeout is None else lifecycle_timeout
        result = run([self.binary, "-C", project, "--json", "--non-interactive", "--no-color",
                      "--timeout", str(budget), *args], env or self.env, timeout, ok=False)
        records = []
        for line in result.stdout.splitlines():
            try:
                record = json.loads(line)
            except ValueError as error:
                raise SmokeFailure(f"CLI corrupted JSON stdout: {line!r}\n{result.stderr}") from error
            require(record.get("schemaVersion") == 1, "unknown result schema version")
            records.append(record)
        require(records, f"CLI produced no structured result: {result.stderr}")
        require(sum(record.get("type") in ("result", "error") for record in records) == 1,
                "CLI emitted more than one terminal envelope")
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
        inputs = {"project": name, "backend": backend, "imagePrefix": name,
                  "hostSecretUid": os.getuid(), "hostSecretGid": os.getgid()}
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
        # Leave this schema-only checkout incomplete: config-only runs need no
        # Docker daemon and must not leak a stopped registration.
        project = self.checkout("config")
        fields = self.cli(project, "config", "schema")["fields"]
        require(any(field["path"] == "oauth.enabled" and field["default"] is False for field in fields),
                "nested schema/default discovery failed without env.yaml")
        require(any(field["path"] == "project" and field["required"] for field in fields),
                "required configuration discovery failed")
        require(not (project / "env.yaml").exists(), "schema inspection created environment")
        plan = self.cli(project, "up", "--plan")
        require(plan.get("sideEffects") is False, "unresolved plan not side-effect free")
        require(not (project / "env.yaml").exists(), "plan executed defaults hook")
        self.config(project, "apiPort", 8123)
        path = project / "env.yaml"
        self.compose_private_file(path, "# smoke comment\n" + path.read_text() + "# smoke trailing comment\n")
        self.config(project, "oauth.enabled", True)
        self.config(project, "apiPort", 8124)
        before = path.read_bytes()
        require(b"# smoke comment" in before and b"# smoke trailing comment" in before,
                "scalar edit lost YAML comments")
        self.cli(project, "config", "set", "apiPort", "70000", fail=True)
        require(path.read_bytes() == before, "invalid candidate overwrote environment")
        require(not self.cli(project, "env", "list")["environments"],
                "incomplete configuration acquired a registry claim")
        self.report.append("schema/incremental edits/comments/invalid candidate/no-effect plan; no configured claim")
    

    def compose_smoke(self):
        first = self.checkout(self.prefix + "-compose-a")
        second = self.checkout(self.prefix + "-compose-b")
        for project in (first, second):
            self.compose_fixture_model(project)
        source = self.root / "private-import.input"
        self.compose_private_file(source, uuid.uuid4().hex.encode())
        name_a = self.compose_bootstrap(first, imported=source)
        shared = first / "env.shared.yaml"
        require(shared.is_file() and shared.stat().st_mode & 0o777 == 0o600,
                "defaults hook did not create private shared settings")
        name_b = self.compose_bootstrap(second, source=shared)
        require(name_a != name_b, "path-hashed proposals collided across worktrees")
        port_a, port_b = self.configured_port(first), self.configured_port(second)
        require(port_a != port_b, "worktrees allocated the same generated endpoint")
        registered = {row["root"]: row for row in self.cli(first, "env", "list", "--worktrees")["environments"]}
        for project, name in ((first, name_a), (second, name_b)):
            require(registered[str(project)]["state"] == "committed" and registered[str(project)]["project"] == name,
                    "complete stopped setup missing from registry")
        self.cli(first, "config", "set", "oauth.issuer", json.dumps("shared-one"), "--shared")
        require(self.cli(second, "config", "get", "oauth.issuer")["value"] == "shared-one",
                "shared change was not live in second checkout")
        self.config(second, "oauth.issuer", "local-override")
        self.cli(first, "config", "set", "oauth.issuer", json.dumps("shared-two"), "--shared")
        require(self.cli(second, "config", "get", "oauth.issuer")["value"] == "local-override",
                "shared edit overwrote explicit local override")
        self.cli(second, "config", "unset", "oauth.issuer")
        require(self.cli(second, "config", "get", "oauth.issuer")["value"] == "shared-two",
                "unsetting override did not reveal inherited value")
        refs = (self.secret_ref(first), self.secret_ref(second))
        require(refs[0] != refs[1], "worktrees share credentials")
        before_plan = (first / "env.yaml").read_bytes()
        self.cli(first, "up", "--plan")
        require((first / "env.yaml").read_bytes() == before_plan and self.secret_ref(first) == refs[0],
                "resolved plan mutated environment or secret")
        blocked = self.checkout(self.prefix + "-blocked")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            occupied = listener.getsockname()[1]
            self.setup(blocked, "blocked", port=occupied)
            error = self.cli(blocked, "up", fail=True, timeout=900)
            require("port" in error["message"].lower() or "address already in use" in error["message"].lower(),
                    "occupied-port startup did not explain conflict")
            require(listener.fileno() >= 0 and listener.getsockname()[1] == occupied,
                    "startup commandeered unrelated listener")
        count, migration = self.compose_once_up(first, name_a, port_a, 0)
        self.cli(second, "up", timeout=900)
        initial, other = self.identity(port_a, name_a), self.identity(port_b, name_b)
        require(initial["migrated"] and other["migrated"], "one-shot migration did not complete")
        require(initial["secretFingerprint"] != other["secretFingerprint"], "worktree secrets are not isolated")
        report = self.cli(first, "status")
        api = next(row for row in report["services"] if row["name"] == "api")
        require(report["ready"] and api["applicationReady"] is True, "strict status did not verify application")
        inspected = self.cli(first, "status", "--inspect-only")
        require(inspected["inspectOnly"] and next(row for row in inspected["services"] if row["name"] == "api")["applicationReady"] is None,
                "inspect-only claimed application readiness")
        self.cli(first, "logs", "--tail", "20", "api")
        self.cli(first, "exec", "api", "--", "python", "-c",
                 "import os,pathlib; assert os.getuid()==1000; assert pathlib.Path('/run/secrets/authKey').read_bytes()")
        self.docker("exec", self.compose_container(name_a, "api"), "python", "-c",
                    "from pathlib import Path; Path('/data/smoke-sentinel').write_text('preserve-me')")
        # Scoped consumer startup still receives the prerequisite safety sequence.
        count, migration = self.compose_once_up(first, name_a, port_a, count, migration, "api")
        self.config(first, "wrongIdentity", True)
        self.compose_status_failure(first, "api", "identity")
        self.config(first, "wrongIdentity", False)
        require(self.cli(first, "status")["ready"], "restoring expected identity did not recover strict status")
        self.config(first, "failHealth", True)
        self.compose_bounded_cli(first, 60, "up", "api", fail=True)
        poll(lambda: self.json_docker("inspect", self.compose_container(name_a, "api"))[0]["State"]["Health"]["Status"],
             lambda value: value == "unhealthy", "deliberate unhealthy application", timeout=45)
        self.compose_status_failure(first, "api", "unhealthy")
        self.config(first, "failHealth", False)
        self.cli(first, "up", timeout=900)
        self.docker("rm", "-f", self.compose_container(name_a, "worker"))
        self.compose_status_failure(first, "worker", "absent")
        self.cli(first, "up", timeout=900)
        seeds = (first / "smoke-seed-count").read_text()
        self.config(first, "failMigration", True)
        error = self.cli(first, "up", "api", fail=True, timeout=900)
        require("migrat" in error["message"].lower(), "prerequisite failure omitted service")
        logs = self.docker("logs", self.compose_container(name_a, "migrate"))
        require("sample migration failed" in logs.stdout + logs.stderr, "failed prerequisite logs not retained")
        self.compose_assert_consumers_stopped(name_a)
        require((first / "smoke-seed-count").read_text() == seeds, "failed prerequisite ran seed")
        self.config(first, "failMigration", False)
        self.cli(first, "up", timeout=900)
        require(self.compose_data(name_a, "smoke-sentinel") == "preserve-me", "failure destroyed data")
        self.compose_cancel_migration(first, name_a)
        self.cli(first, "up", timeout=900)
        require(self.compose_data(name_a, "smoke-sentinel") == "preserve-me", "cancellation destroyed data")
        volumes = self.docker("volume", "ls", "--quiet", "--filter", "label=com.docker.compose.project=" + name_a).stdout.split()
        require(volumes, "sample did not create application data")
        self.cli(first, "ports", "release", "--yes", fail=True)
        self.cli(first, "env", "forget", str(first), "--yes", fail=True)
        self.cli(first, "down")
        for volume in volumes:
            self.docker("volume", "inspect", volume)
        require(self.secret_ref(first) == refs[0] and self.configured_port(first) == port_a,
                "ordinary down reset credentials/endpoints")
        self.cli(first, "up", timeout=900)
        require(self.identity(port_a, name_a)["secretFingerprint"] == initial["secretFingerprint"],
                "ordinary restart regenerated credential")
        require(self.compose_data(name_a, "smoke-sentinel") == "preserve-me", "down/up lost application data")
        self.watch_smoke(first, port_a, name_a)
        self.compose_private_file(source, uuid.uuid4().hex.encode())
        self.cli(first, "up", timeout=900)
        require(self.secret_ref(first) == refs[0] and self.identity(port_a, name_a)["secretFingerprint"] == initial["secretFingerprint"],
                "ordinary startup implicitly synced an imported source")
        deferred = self.cli(first, "secrets", "sync", "authKey", "--plan")
        require(deferred["comparisonsDeferred"] and deferred["secrets"][0]["status"] == "comparison-deferred",
                "sync plan falsely claimed a content comparison")
        rendered_old = self.cli(first, "render", "--target", "compose")
        synced = self.cli(first, "secrets", "sync", "authKey", "--yes")
        new_ref = self.secret_ref(first)
        require(synced["committed"] == ["authKey"] and synced["secrets"][0]["status"] == "replaced" and new_ref != refs[0],
                "explicit sync did not publish a fresh immutable revision")
        require(synced["secrets"][0]["source"]["canonicalPath"] == str(source.resolve()), "sync changed source origin")
        require(self.identity(port_a, name_a)["secretFingerprint"] == initial["secretFingerprint"],
                "storage-only sync implicitly restarted consumers")
        # Refresh the executable render snapshot without touching live mounts,
        # so this refusal proves live consumption rather than stale rendering.
        self.cli(first, "logs", "--tail", "1", "api")
        self.compose_gc_refusal(first, refs[0], "consumer")
        unchanged = self.cli(first, "secrets", "sync", "authKey", "--yes")
        require(unchanged["committed"] == [] and unchanged["secrets"][0]["status"] == "unchanged" and self.secret_ref(first) == new_ref,
                "unchanged source created another revision")
        self.cli(first, "down")
        snapshot = first / ".dockstride" / "smoke-retained.json"
        self.compose_private_file(snapshot, json.dumps(rendered_old))
        self.compose_gc_refusal(first, refs[0], "snapshot")
        snapshot.unlink()
        self.config(second, "authInput", refs[0]["file"])
        self.compose_gc_refusal(first, refs[0], "source")
        require(self.cli(second, "config", "get", "authInput")["value"] == refs[0]["file"],
                "GC protection rewrote declared source field")
        self.cli(first, "up", timeout=900)
        require(self.identity(port_a, name_a)["secretFingerprint"] != initial["secretFingerprint"],
                "explicit down/up did not remount synchronized storage")
        self.compose_postgres_smoke()
        other_ids = {service: self.json_docker("inspect", self.compose_container(name_b, service))[0]["Id"]
                     for service in ("api", "worker", "migrate")}
        other_saved = next(row for row in self.cli(second, "env", "list")["environments"] if row["root"] == str(second))
        self.cli(first, "destroy", "--plan")
        self.cli(first, "destroy", "--yes")
        for volume in volumes:
            require(self.docker("volume", "inspect", volume, ok=False).returncode != 0, "destroy preserved data volume")
        require(self.secret_ref(first) == new_ref, "destroy removed immutable credential")
        retained_yaml = (first / "env.yaml").read_bytes()
        release_plan = self.cli(first, "ports", "release", "--plan")
        require(release_plan["plan"] and not release_plan["blockers"] and
                len(release_plan["reservations"]) == 1 and
                release_plan["fields"] == [{"field": "apiPort", "port": port_a,
                                           "key": release_plan["reservations"][0]["key"]}],
                "release plan did not identify the exact generated endpoint")
        require((first / "env.yaml").read_bytes() == retained_yaml, "release plan changed local settings")
        released = self.cli(first, "ports", "release", "--yes")
        require(released["released"] == 1, "release did not clear exact owned reservation")
        require("apiPort:" not in (first / "env.yaml").read_text(), "release retained generated local endpoint")
        self.cli(first, "env", "forget", str(first), "--plan")
        self.cli(first, "env", "forget", str(first), "--yes")
        require(not any(row["root"] == str(first) for row in self.cli(second, "env", "list")["environments"]),
                "explicit forget retained fixture registration")
        self.cli(first, "setup")
        self.cli(first, "up", timeout=900)
        self.identity(self.configured_port(first), name_a)
        require(self.secret_ref(first) == new_ref, "recreated fixture regenerated retained credential")
        for service, expected in other_ids.items():
            require(self.json_docker("inspect", self.compose_container(name_b, service))[0]["Id"] == expected,
                    "destroy/release/forget/recreate altered another checkout")
        saved = next(row for row in self.cli(second, "env", "list")["environments"] if row["root"] == str(second))
        require(all(saved[key] == other_saved[key] for key in ("ownerId", "project", "allocations", "allocatedEndpoints")),
                "fixture endpoint reset disturbed another checkout's claims")
        require(self.identity(port_b, name_b)["secretFingerprint"] == other["secretFingerprint"],
                "recreate affected another checkout's credential")
        self.report.append("Compose: two defaults/shared-source worktrees, stopped claims/ports/overrides; strict identity/unhealthy/absent status; fresh migration event+identity+marker proof, failure/cancellation/seed safety; non-root secrets/watch/logs/exec/data; explicit sync/no-op/live/snapshot/source GC protection; isolated destroy/release/forget/recreate")
    

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
        self.host_daemon_id = self.docker("info", "--format", "{{.ID}}", env=self.host_env).stdout.strip()
        require(self.host_daemon_id, "outer Docker daemon has no verifiable identity")
        self.network = self.prefix + "-network"
        self.docker("network", "create", "--label", "dockstride.smoke=" + self.prefix,
                    self.network, env=self.host_env)
        registry = self.prefix + "-registry"
        self.containers.append(registry)
        result = self.docker("run", "-d", "--name", registry, "--network", self.network,
                             "--network-alias", "registry", "--label", "dockstride.smoke=" + self.prefix,
                             self.args.registry_image, env=self.host_env)
        self.outer_container_ids[registry] = result.stdout.strip()
        self.app_port = reserve_port()
        for role in ("manager", "worker"):
            name = self.prefix + "-" + role
            api = reserve_port()
            data = self.root / ("dind-data-" + role)
            data.mkdir(mode=0o700)
            self.dind_data_paths.append(data)
            self.containers.append(name)
            argv = ["run", "-d", "--privileged", "--name", name, "--network", self.network,
                    "--label", "dockstride.smoke=" + self.prefix, "-e", "DOCKER_TLS_CERTDIR=",
                    "-p", f"127.0.0.1:{api}:2375",
                    "--mount", f"type=bind,source={data},target=/var/lib/docker"]
            if role == "manager":
                argv += ["-p", f"127.0.0.1:{self.app_port}:{self.app_port}"]
            argv += [self.args.dind_image, "--host=tcp://0.0.0.0:2375", "--host=unix:///var/run/docker.sock",
                     "--insecure-registry=registry:5000", "--userland-proxy=true"]
            result = self.docker(*argv, env=self.host_env)
            self.outer_container_ids[name] = result.stdout.strip()
            context = self.prefix + "-" + role
            self.contexts.append(context)
            environment = self.host_env.copy()
            environment.pop("DOCKER_HOST", None)
            environment.pop("DOCKER_TLS_VERIFY", None)
            environment.pop("DOCKER_CERT_PATH", None)
            environment["DOCKER_CONTEXT"] = context
            self.node_envs[role] = environment
            self.dind_nodes[role] = {
                "outerId": self.outer_container_ids[name], "context": context,
                "endpoint": f"tcp://127.0.0.1:{api}", "apiPort": str(api),
                "data": data, "daemonId": None, "nodeId": None,
            }
            self.docker("context", "create", context, "--docker", f"host=tcp://127.0.0.1:{api}", env=self.host_env)
            try:
                poll(lambda: self.docker("info", env=environment, ok=False, timeout=10),
                     lambda result: result.returncode == 0, role + " disposable DIND daemon", timeout=120)
            except SmokeFailure as error:
                logs = self.docker("logs", name, env=self.host_env, ok=False).stdout
                raise SmokeFailure(f"host cannot start isolated DIND {role}; Swarm was NOT verified.\n{logs}\n{error}") from error
            info = self.json_docker("info", "--format", "{{json .}}", env=environment)
            require(info.get("ID") and info["ID"] != self.host_daemon_id and
                    all(info["ID"] != node["daemonId"] for other, node in self.dind_nodes.items()
                        if other != role) and info["Swarm"]["LocalNodeState"] == "inactive",
                    "new disposable DIND daemon is not an independent inactive node")
            self.dind_nodes[role]["daemonId"] = info["ID"]
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
        swarm = self.verified_dind_node("manager")["Swarm"]
        require(swarm.get("LocalNodeState") == "active" and swarm.get("ControlAvailable") and
                swarm.get("NodeID") and swarm.get("Cluster", {}).get("ID"),
                "disposable manager initialization did not establish its exact cluster identity")
        self.swarm_identity = {"manager": swarm["NodeID"], "cluster": swarm["Cluster"]["ID"]}
        self.dind_nodes["manager"]["nodeId"] = swarm["NodeID"]

    def swarm_smoke(self):
        self.dind_fixture()
        project = self.checkout("swarm")
        name = self.setup(project, "swarm", "swarm", self.app_port)
        secret_before = self.secret_ref(project)
        generated = self.swarm_secret(project, name, secret_before)
        revision_id = generated["Spec"]["Labels"]["io.dockstride.revision"]
        recovery = self.root / "recovery" / ("swarm-" + revision_id)
        metadata = recovery.stat()
        require(recovery.is_file() and not recovery.is_symlink() and metadata.st_uid == os.getuid() and
                metadata.st_mode & 0o777 == 0o600 and metadata.st_size > 0,
                "durable generated Swarm secret has no private revision-specific recovery file")
        before_plan = (project / "env.yaml").read_bytes()
        self.cli(project, "deploy", "--plan")
        require((project / "env.yaml").read_bytes() == before_plan and self.secret_ref(project) == secret_before,
                "Swarm plan mutated environment or secret")
        self.cli(project, "deploy", timeout=900)
        identity = self.identity(self.app_port, name)
        require(len(self.docker("node", "ls", "-q").stdout.split()) == 1, "fixture was not single-node")
        for service in ("api", "worker"):
            require("@sha256:" in self.swarm_service(name, service)["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"],
                    "single-node Swarm did not deploy immutable image digests")
        self.cli(project, "deploy", timeout=900)
        require(self.secret_ref(project) == secret_before and
                self.identity(self.app_port, name)["secretFingerprint"] == identity["secretFingerprint"],
                "repeated deployment regenerated Swarm secret")
        token = self.docker("swarm", "join-token", "-q", "worker").stdout.strip()
        self.docker("swarm", "join", "--token", token, self.manager_address + ":2377", env=self.node_envs["worker"])
        swarm = self.verified_dind_node("worker")["Swarm"]
        require(swarm.get("LocalNodeState") == "active" and swarm.get("NodeID"),
                "disposable worker join did not establish its exact node identity")
        self.dind_nodes["worker"]["nodeId"] = swarm["NodeID"]
        poll(lambda: self.docker("node", "ls", "--format", "{{.Status}} {{.Availability}}").stdout,
             lambda text: text.count("Ready Active") == 2, "two-node disposable Swarm")
        self.config(project, "workerOnSeparateNode", True)
        self.cli(project, "deploy", timeout=900)
        worker_spec = self.swarm_service(name, "worker")
        require("@sha256:" in worker_spec["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"],
                "Swarm build did not deploy immutable digest")
        worker_id = poll(lambda: self.docker("ps", "-q", "--filter", "label=com.docker.swarm.service.name=" + name + "_worker",
                                             env=self.node_envs["worker"]).stdout.strip(), bool, "worker-node application task")
        proof = self.docker("exec", worker_id, "python", "-c",
                            "import json,urllib.request; print(json.dumps(json.load(urllib.request.urlopen('http://127.0.0.1:8000/health'))))",
                            env=self.node_envs["worker"])
        worker_identity = json.loads(proof.stdout)
        require(worker_identity["project"] == name and worker_identity["uid"] == 1000 and
                worker_identity["secretFingerprint"] == identity["secretFingerprint"],
                "worker did not pull/read the same image/secret from shared registry and Swarm")
        require(self.secret_ref(project) == secret_before, "two-node deployment regenerated secret")
        revision = "selected-" + uuid.uuid4().hex[:8]
        self.swarm_private_write(project / "api" / "content" / "revision.txt", revision + "\n")
        self.cli(project, "deploy", "api", timeout=900)
        selected_identity = self.identity(self.app_port, name, revision)
        require(self.secret_ref(project) == secret_before and
                selected_identity["secretFingerprint"] == identity["secretFingerprint"],
                "selected deployment regenerated application credential")
        after = self.swarm_service(name, "worker")
        require(after["Version"] == worker_spec["Version"] and after["Spec"] == worker_spec["Spec"],
                "selected API deploy modified unrelated worker service")
        self.swarm_strict_status(project, name)
        self.cli(project, "logs", "--tail", "20", "api")
        unsupported = self.cli(project, "exec", "api", "true", fail=True)
        require("compose" in unsupported.get("message", "").lower(), "Swarm exec lost its node-local task boundary")
        api_id = poll(lambda: self.docker("ps", "-q", "--filter", "label=com.docker.swarm.service.name=" + name + "_api").stdout.strip(),
                      bool, "owned manager API task")
        self.docker("exec", api_id, "python", "-c", "from pathlib import Path; Path('/data/smoke-preserved').write_text('retained')")
        self.config(project, "failHealth", True)
        error = self.cli(project, "deploy", "api", fail=True, timeout=900)
        require(any(word in error["message"].lower() for word in ("rollout", "task", "health", "converge", "pause")),
                "failed rollout lacks actionable task/health/convergence error")
        row = self.swarm_failed_status(project, "api")
        require(row.get("containerReady") is False and row.get("ready") is False,
                "failed rollout did not fail strict status")
        self.config(project, "failHealth", False)
        self.cli(project, "deploy", "api", timeout=900)
        self.identity(self.app_port, name, revision)
        require(self.cli(project, "status").get("ready") is True, "failed rollout restoration did not recover")
        api_id = poll(lambda: self.docker("ps", "-q", "--filter", "label=com.docker.swarm.service.name=" + name + "_api").stdout.strip(),
                      bool, "restored manager API task")
        self.docker("exec", api_id, "python", "-c", "from pathlib import Path; assert Path('/data/smoke-preserved').read_text() == 'retained'")
        self.cli(project, "down")
        require(self.secret_ref(project) == secret_before and self.configured_port(project) == self.app_port,
                "Swarm down changed stable credential/endpoint")
        self.cli(project, "status", fail=True)
        refused = self.cli(project, "destroy", "--yes", fail=True)
        require("multi-node" in refused.get("message", "").lower(),
                "multi-node destroy did not preserve conservative node-local volume boundary")
        self.retire_swarm_worker()
        self.cli(project, "destroy", "--yes")
        require(self.secret_ref(project) == secret_before and recovery.stat().st_size == metadata.st_size,
                "destroy discarded durable generated secret/recovery")
        self.swarm_secret(project, name, secret_before)
        self.cli(project, "ports", "release", "--yes")
        self.report.append("disposable single-node/two-node Swarm: durable generated recovery, published immutable digests distributed to worker, repeated/scoped deploy, strict native/application status, missing service/replica faults and restoration, inspect-only, logs/node-local exec boundary, failed rollout and retained data, conservative multi-node destroy and owned single-node teardown")
        self.swarm_import_sync_smoke()
    

    def verified_outer_container(self, name):
        result = self.docker("inspect", "--type", "container", name, env=self.host_env, ok=False)
        if result.returncode:
            missing = {
                "error: no such object: " + name.lower(),
                "error: no such container: " + name.lower(),
                "error response from daemon: no such container: " + name.lower(),
            }
            require(result.returncode == 1 and result.stderr.strip().lower() in missing,
                    "cannot prove outer fixture container ownership: " + name)
            return None
        item = json.loads(result.stdout)[0]
        require(item.get("Id") == self.outer_container_ids.get(name) and
                item.get("Name") == "/" + name and
                item.get("Config", {}).get("Labels", {}).get("dockstride.smoke") == self.prefix,
                "outer fixture container ownership changed: " + name)
        return item

    def verified_dind_node(self, role):
        record = self.dind_nodes[role]
        environment = self.node_envs[role]
        require(environment.get("DOCKER_CONTEXT") == record["context"] and
                record["context"] in self.contexts and not environment.get("DOCKER_HOST"),
                "DIND node connection changed: " + role)
        context = self.json_docker("context", "inspect", record["context"], env=self.host_env)[0]
        require(context.get("Name") == record["context"] and
                context.get("Endpoints", {}).get("docker", {}).get("Host") == record["endpoint"],
                "DIND context no longer targets its owned loopback endpoint: " + role)
        outer = self.verified_outer_container(self.prefix + "-" + role)
        require(outer and outer["State"]["Running"] and outer["Id"] == record["outerId"] and
                outer.get("NetworkSettings", {}).get("Ports", {}).get("2375/tcp") ==
                [{"HostIp": "127.0.0.1", "HostPort": record["apiPort"]}] and
                any(mount.get("Type") == "bind" and mount.get("Source") == str(record["data"]) and
                    mount.get("Destination") == "/var/lib/docker" for mount in outer.get("Mounts", [])),
                "DIND outer node or private storage binding changed: " + role)
        info = self.json_docker("info", "--format", "{{json .}}", env=environment)
        require(record["daemonId"] and info.get("ID") == record["daemonId"] and
                info["ID"] != self.host_daemon_id,
                "DIND daemon identity cannot be proven: " + role)
        return info

    def verified_swarm_nodes(self):
        observed = {role: self.verified_dind_node(role) for role in self.dind_nodes}
        if not self.swarm_identity:
            require(all(info["Swarm"]["LocalNodeState"] == "inactive" for info in observed.values()),
                    "unrecorded DIND Swarm identity; retaining recovery")
            return None
        manager = observed["manager"]["Swarm"]
        require(manager.get("LocalNodeState") == "active" and manager.get("ControlAvailable") and
                manager.get("NodeID") == self.swarm_identity["manager"] and
                manager.get("Cluster", {}).get("ID") == self.swarm_identity["cluster"],
                "disposable manager cluster identity changed")
        worker = observed["worker"]["Swarm"]
        worker_record = self.dind_nodes["worker"]
        if worker.get("LocalNodeState") == "active":
            require(not worker.get("ControlAvailable") and worker.get("NodeID") and
                    {remote.get("NodeID") for remote in worker.get("RemoteManagers", [])} ==
                    {self.swarm_identity["manager"]} and
                    worker_record["nodeId"] in (None, worker["NodeID"]),
                    "disposable worker membership or exact node identity changed")
            # Join can succeed before an interrupted caller records its node ID.
            # The pinned daemon, outer container, and manager membership prove it.
            worker_record["nodeId"] = worker["NodeID"]
        else:
            require(worker.get("LocalNodeState") == "inactive" and not worker.get("NodeID"),
                    "disposable worker has an unprovable departure state")
        environment = self.node_envs["manager"]
        ids = self.docker("node", "ls", "-q", env=environment).stdout.split()
        allowed = {self.swarm_identity["manager"]}
        if worker_record["nodeId"]:
            allowed.add(worker_record["nodeId"])
        require(set(ids) <= allowed and self.swarm_identity["manager"] in ids,
                "unknown or missing nodes in disposable manager cluster; retaining recovery")
        nodes = {}
        for node_id in ids:
            node = self.json_docker("node", "inspect", node_id, env=environment)[0]
            expected_role = "manager" if node_id == self.swarm_identity["manager"] else "worker"
            require(node.get("ID") == node_id and node.get("Spec", {}).get("Role") == expected_role,
                    "disposable cluster node identity or role changed")
            nodes[node_id] = node
        require(worker.get("LocalNodeState") != "active" or worker["NodeID"] in nodes,
                "active disposable worker is missing from its verified manager")
        return nodes

    def retire_swarm_worker(self):
        nodes = self.verified_swarm_nodes()
        if nodes is None:
            return
        environment = self.node_envs["manager"]
        require(not self.docker("service", "ls", "-q", env=environment).stdout.split(),
                "services remain in disposable cluster before worker retirement")
        worker_id = self.dind_nodes["worker"]["nodeId"]
        if worker_id in nodes:
            worker = self.verified_dind_node("worker")["Swarm"]
            if worker["LocalNodeState"] == "active":
                require(worker["NodeID"] == worker_id, "worker identity changed before leave")
                self.docker("swarm", "leave", "--force", env=self.node_envs["worker"])
            poll(lambda: self.docker("node", "inspect", "--format", "{{.Status.State}}",
                                    worker_id, env=environment).stdout.strip(),
                 lambda state: state == "down", "owned DIND worker leave")
            # Recheck all cluster identities after waiting, before exact node removal.
            self.verified_swarm_nodes()
            self.docker("node", "rm", worker_id, env=environment)
        require(set(self.verified_swarm_nodes()) == {self.swarm_identity["manager"]},
                "owned worker node was not retired")

    def cleanup(self):
        errors = []
        if self.watch and self.watch.poll() is None:
            os.killpg(self.watch.pid, signal.SIGTERM)
            self.watch.wait(timeout=20)
        environments = {str(Path(path).resolve()): env for path, _, env in self.projects}
        inventory = self.cli(self.repo, "env", "list", env=self.host_env)
        fixtures = []
        if inventory["pendingOperations"]:
            errors.append("fixture pending publication claims require recovery")
        for entry in reversed(inventory["environments"]):
            project = Path(entry["root"])
            try:
                require(project.resolve().is_relative_to(self.root.resolve()),
                        "refusing cleanup of a registration outside this fixture")
                identity = json.loads((project / ".dockstride" / "identity.json").read_text())
                owner, name = identity["id"], entry["project"]
                require(identity["root"] == str(project.resolve()) and
                        identity["project"] == name and entry["ownerId"] == owner,
                        "fixture cleanup ownership identity changed")
                require(str(project.resolve()) in environments,
                        "refusing cleanup of an untracked fixture registration")
                environment = environments[str(project.resolve())]
                if entry["backend"] == "swarm":
                    require(environment == self.node_envs.get("manager"),
                            "refusing Swarm cleanup outside the pinned disposable manager")
                fixtures.append((entry, project, owner, name, environment))
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append(f"{project}: {error}")
        if not errors:
            try:
                # Prove both outer owners and the complete cluster before any down.
                # Stop every registered fixture first, not just the first project,
                # so retiring its worker cannot disrupt another owned deployment.
                self.verified_swarm_nodes()
                for entry, project, _, _, environment in fixtures:
                    if entry["backend"] == "swarm":
                        self.cli(project, "down", env=environment, timeout=180)
                self.retire_swarm_worker()
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append("disposable Swarm retirement: " + str(error))
        if errors:
            raise SmokeFailure("fixture cleanup failed; recovery retained at " + str(self.root) +
                               ":\n" + "\n".join(errors))
        for entry, project, owner, name, environment in fixtures:
            try:
                self.cli(project, "destroy", "--yes", env=environment, timeout=180)
                self.cli(project, "ports", "release", "--yes", env=environment, timeout=120)
                if entry["backend"] == "swarm":
                    # Current immutable backend objects deliberately survive destroy.
                    # Only exact owned objects in this disposable daemon are removed.
                    require(environment.get("DOCKER_CONTEXT") in self.contexts,
                            "refusing Swarm secret teardown outside disposable DIND")
                    service_ids = self.docker("service", "ls", "-q", "--filter",
                                              "label=io.dockstride.owner=" + owner,
                                              env=environment).stdout.split()
                    require(not service_ids, "Swarm services remain before secret teardown")
                    secret_ids = self.docker("secret", "ls", "-q", "--filter",
                                             "label=io.dockstride.owner=" + owner,
                                             "--filter", "label=io.dockstride.project=" + name,
                                             env=environment).stdout.split()
                    for secret_id in secret_ids:
                        item = self.json_docker("secret", "inspect", secret_id, env=environment)[0]
                        labels = item["Spec"].get("Labels", {})
                        require(item["ID"] == secret_id and
                                labels.get("io.dockstride.owner") == owner and
                                labels.get("io.dockstride.project") == name,
                                "immutable secret teardown ownership changed")
                        self.docker("secret", "rm", secret_id, env=environment)
                forgotten = self.cli(project, "env", "forget", str(project), "--yes",
                                     env=environment, timeout=120)
                require(forgotten.get("forgotten") is True,
                        "fixture environment registration was not retired")
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append(f"{project}: {error}")
        remaining = self.cli(self.repo, "env", "list", env=self.host_env)
        if remaining["environments"] or remaining["pendingOperations"]:
            errors.append("fixture registry or pending publication claims remain")
        reservations_path = self.root / "home" / ".local/share/dockstride/.dockstride/port-reservations.json"
        if reservations_path.exists():
            reservations = json.loads(reservations_path.read_text())
            if reservations.get("reservations"):
                errors.append("fixture endpoint reservations remain")
        if errors:
            # Preserve the pinned DIND connection and private configuration needed
            # for recovery; never hide leaked claims by deleting isolated HOME.
            raise SmokeFailure("fixture cleanup failed; recovery retained at " + str(self.root) +
                               ":\n" + "\n".join(errors))
        for name in reversed(self.containers):
            try:
                item = self.verified_outer_container(name)
                if item:
                    result = self.docker("rm", "-f", "-v", item["Id"], env=self.host_env, ok=False)
                    if result.returncode:
                        errors.append(result.stderr)
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append(f"{name}: {error}")
        if not errors:
            for data in self.dind_data_paths:
                require(data.parent == self.root and not data.is_symlink(),
                        "refusing DIND storage cleanup outside this fixture")
                # Stop the daemons first. Their mapped-UID files require a helper
                # in the same user namespace; never prune host Docker storage.
                result = self.docker(
                    "run", "--rm", "--privileged", "--network", "none", "--user", "0:0",
                    "--label", "dockstride.smoke=" + self.prefix, "--entrypoint", "sh",
                    "--mount", f"type=bind,source={data},target=/var/lib/docker",
                    self.args.dind_image, "-c",
                    "rm -rf -- /var/lib/docker/* /var/lib/docker/.[!.]* /var/lib/docker/..?*",
                    env=self.host_env, ok=False, timeout=120)
                if result.returncode:
                    errors.append(result.stderr)
                else:
                    data.rmdir()
        if errors:
            raise SmokeFailure("outer fixture cleanup failed; recovery retained at " + str(self.root) +
                               ":\n" + "\n".join(errors))
        if self.network:
            item = self.json_docker("network", "inspect", self.network, env=self.host_env)[0]
            require(item.get("Name") == self.network and
                    item.get("Labels", {}).get("dockstride.smoke") == self.prefix,
                    "outer fixture network ownership changed; retaining recovery")
            result = self.docker("network", "rm", item["Id"], env=self.host_env, ok=False)
            if result.returncode:
                raise SmokeFailure("outer network cleanup failed; recovery retained at " + str(self.root) +
                                   ":\n" + result.stderr)
        for name in reversed(self.contexts):
            result = self.docker("context", "rm", "-f", name, env=self.host_env, ok=False)
            if result.returncode:
                errors.append(result.stderr)
        if errors:
            raise SmokeFailure("outer fixture cleanup failed:\n" + "\n".join(errors))
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

    def compose_private_file(self, path, content):
        path.parent.mkdir(parents=True, exist_ok=True)
        flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW
        descriptor = os.open(path, flags, 0o600)
        with os.fdopen(descriptor, "wb") as output:
            os.fchmod(output.fileno(), 0o600)
            output.write(content.encode() if isinstance(content, str) else content)
        require(path.stat().st_uid == os.getuid(), "fixture input is not owned by invoking UID")


    def compose_terminal(self, result, fail=False):
        records = [json.loads(line) for line in result.stdout.splitlines()]
        require(records and all(row.get("schemaVersion") == 1 for row in records),
                "bounded CLI produced invalid structured output")
        terminals = [row for row in records if row.get("type") in ("result", "error")]
        require(len(terminals) == 1 and terminals[0] is records[-1],
                "bounded CLI did not emit exactly one terminal envelope")
        terminal = terminals[0]
        if fail:
            require(result.returncode != 0 and terminal.get("type") == "error",
                    "expected bounded lifecycle failure")
            require(terminal.get("exitCode") == result.returncode, "bounded CLI exit code disagrees")
            return terminal
        require(result.returncode == 0 and terminal.get("type") == "result",
                "bounded lifecycle did not succeed")
        return terminal["result"]


    def compose_bounded_cli(self, project, seconds, *args, fail=False):
        return self.cli(project, *args, fail=fail, timeout=seconds + 30, lifecycle_timeout=seconds)


    def compose_bootstrap(self, project, source=None, imported=None):
        # Track the pinned environment before setup, including partial publication.
        # The hook consumes Dockstride's proposal; this harness does not reimplement it.
        self.projects.append((project, None, self.env.copy()))
        if source is not None:
            self.cli(project, "config", "sources", "add", str(source))
        options = self.json_docker("info", "--format", "{{json .SecurityOptions}}")
        gid = 0 if any("rootless" in item for item in options) else os.getgid()
        inputs = {"hostSecretUid": os.getuid(), "hostSecretGid": os.getgid(), "containerGid": gid}
        argv = ["setup"]
        for field, value in inputs.items():
            argv.extend(["--set", field + "=" + json.dumps(value)])
        if imported is not None:
            argv.extend(["--secret-file", "authKey=" + str(imported)])
        self.cli(project, *argv)
        name = self.cli(project, "config", "get", "project")["value"]
        self.projects[-1] = (project, name, self.env.copy())
        self.config(project, "imagePrefix", name)
        require(not self.docker("ps", "-aq", "--filter", "label=com.docker.compose.project=" + name).stdout.strip(),
                "setup unexpectedly started containers")
        return name


    def compose_fixture_model(self, project):
        path = project / "compose.ncl"
        model = path.read_text()
        model = model.replace("  failMigration | Bool | default = false,", "  failMigration | Bool | default = false,\n"
                              "  migrationDelay | Number | default = 0,\n"
                              "  wrongIdentity | Bool | default = false,\n"
                              "  authInput | String | default = \"\",")
        model = model.replace('json = { application = "dockstride-sample", project = env.project, uid = env.containerUid },',
                              'json = { application = "dockstride-sample", project = if env.wrongIdentity then "wrong-fixture-identity" else env.project, uid = env.containerUid },')
        model = model.replace("secrets.authKey = lib.GenerateSecret", "secrets.authKey = if env.authInput != \"\" then lib.FileSecret env.authInput else lib.GenerateSecret")
        model = model.replace("environment = dc.Env { FAIL_MIGRATION = env.failMigration },",
                              "environment = dc.Env { FAIL_MIGRATION = env.failMigration, MIGRATION_DELAY = env.migrationDelay },")
        model = model.replace("    ] else [],", "      { name = \"smoke-seed\", kind = \"command\", argv = [\"python3\", \"smoke-seed.py\"],\n"
                              "        workflows = [\"up\", \"dev\"], stage = \"after\", services = [\"api\", \"worker\", \"migrate\"] },\n"
                              "    ] else [],")
        self.compose_private_file(path, model)
        migration = project / "api" / "migrate.py"
        script = migration.read_text().replace("import sys", "import sys\nimport time")
        script = script.replace("if os.environ.get(\"FAIL_MIGRATION\")", "print('controlled migration delay', flush=True)\ntime.sleep(float(os.environ.get('MIGRATION_DELAY', '0')))\nif os.environ.get(\"FAIL_MIGRATION\")")
        self.compose_private_file(migration, script)
        self.compose_private_file(project / "smoke-seed.py", "from pathlib import Path\np=Path('smoke-seed-count')\np.write_text(str(int(p.read_text())+1 if p.exists() else 1))\n")


    def compose_data(self, name, field):
        container = self.compose_container(name, "api")
        return self.docker("exec", container, "python", "-c",
                           "from pathlib import Path; print(Path('/data/' + " + repr(field) + ").read_text().strip())").stdout.strip()


    def compose_start_events(self, name, start, end):
        result = self.docker("events", "--since", start, "--until", end,
                             "--filter", "type=container", "--filter", "event=start",
                             "--filter", "label=com.docker.compose.project=" + name,
                             "--filter", "label=com.docker.compose.service=migrate", "--format", "{{json .}}")
        return [json.loads(line)["Actor"]["ID"] for line in result.stdout.splitlines()]


    def compose_once_up(self, project, name, port, previous_count, previous_id=None, *services):
        # Docker timestamps, rather than wall-clock sleeps, bound the real event
        # evidence. Nanosecond timestamps prevent adjacent invocations overlap.
        ns = time.time_ns()
        start = f"{ns // 1_000_000_000}.{ns % 1_000_000_000:09d}"
        self.cli(project, "up", *services, timeout=900, lifecycle_timeout=900)
        ns = time.time_ns()
        end = f"{ns // 1_000_000_000}.{ns % 1_000_000_000:09d}"
        migration = self.json_docker("inspect", self.compose_container(name, "migrate"))[0]
        full_id = migration["Id"]
        require(migration["State"]["Status"] == "exited" and migration["State"]["ExitCode"] == 0,
                "native prerequisite did not retain its successful container")
        require(previous_id is None or full_id != previous_id, "new invocation reused old prerequisite identity")
        events = self.compose_start_events(name, start, end)
        require(events == [full_id], "fresh prerequisite did not start exactly once: " + repr(events))
        require(int(self.compose_data(name, "migration-runs")) == previous_count + 1,
                "migration marker proves a missing or duplicated execution")
        self.identity(port, name)
        return previous_count + 1, full_id


    def compose_status_failure(self, project, service, kind):
        error = self.cli(project, "status", fail=True)
        report = error.get("details", {}).get("status")
        require(isinstance(report, dict) and report.get("ready") is False,
                "strict failure lost full details.status report")
        require({row["name"] for row in report["services"]} >= {"api", "worker", "migrate"},
                "failed status omitted service observations")
        row = next(row for row in report["services"] if row["name"] == service)
        require(row["required"] and not row["ready"], "failed required service was reported ready")
        if kind == "identity":
            require(row["containerReady"] and row["applicationReady"] is False,
                    "wrong application identity was not distinguished from container readiness")
        elif kind == "unhealthy":
            require(any(item.get("health") == "unhealthy" for item in row["containers"]),
                    "unhealthy status omitted Docker health evidence")
        elif kind == "absent":
            require(not row["containers"], "absent service had invented container observations")
        return report


    def compose_assert_consumers_stopped(self, name):
        for service in ("api", "worker"):
            item = self.json_docker("inspect", self.compose_container(name, service))[0]
            require(not item["State"]["Running"], "unsafe consumer resumed after prerequisite failure: " + service)


    def compose_cancel_migration(self, project, name):
        self.config(project, "migrationDelay", 120)
        seeds = (project / "smoke-seed-count").read_text()
        previous = self.json_docker("inspect", self.compose_container(name, "migrate"))[0]["Id"]
        stdout_path, stderr_path = project / "cancellation.stdout", project / "cancellation.stderr"
        for path in (stdout_path, stderr_path):
            self.compose_private_file(path, b"")
        with stdout_path.open("w") as stdout_file, stderr_path.open("w") as stderr_file:
            process = subprocess.Popen([self.binary, "-C", str(project), "--json", "--non-interactive", "--no-color",
                                        "--timeout", "240", "up", "api"], env=self.env,
                                       stdout=stdout_file, stderr=stderr_file, text=True, start_new_session=True)
            def running_migration():
                ids = self.docker("ps", "-q", "--filter", "label=com.docker.compose.project=" + name,
                                  "--filter", "label=com.docker.compose.service=migrate").stdout.split()
                if len(ids) != 1:
                    return None
                item = self.json_docker("inspect", ids[0])[0]
                return item if item["Id"] != previous and "controlled migration delay" in self.docker("logs", ids[0]).stdout else None
            try:
                poll(running_migration, lambda item: item is not None, "controlled fresh migration running", timeout=90)
                self.compose_assert_consumers_stopped(name)
                os.killpg(process.pid, signal.SIGINT)
                process.wait(timeout=30)
                result = subprocess.CompletedProcess(process.args, process.returncode,
                                                     stdout_path.read_text(), stderr_path.read_text())
                self.compose_terminal(result, fail=True)
                require(process.returncode == 130, "cancelled prerequisite did not preserve interruption status")
                self.compose_assert_consumers_stopped(name)
                require((project / "smoke-seed-count").read_text() == seeds, "cancelled migration ran after-ready seed")
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=20)
        # Cancellation intentionally retains evidence/resources, not automatic
        # recovery. A separate up below is the explicit recovery choice.
        self.config(project, "migrationDelay", 0)


    def compose_gc_refusal(self, project, reference, expected_reason):
        plan = self.cli(project, "secrets", "gc", reference["file"], "--plan")["plan"]
        require(len(plan) == 1 and plan[0]["eligible"] is False,
                "protected immutable revision became eligible for GC")
        require(expected_reason in plan[0]["reason"].lower(),
                "GC did not explain the relevant protection: " + plan[0]["reason"])
        self.cli(project, "secrets", "gc", plan[0]["revision"], "--yes", fail=True)
        require(Path(reference["file"]).is_file(), "GC refusal nevertheless deleted storage")


    def compose_postgres_smoke(self):
        project = self.checkout(self.prefix + "-postgres")
        name = self.prefix + "-postgres"
        self.projects.append((project, name, self.env.copy()))
        password = self.root / "postgres.input"
        self.compose_private_file(password, uuid.uuid4().hex + "\n")
        model = r'''let lib = import "libs/dockstride.ncl" in
let cfg = {
  project | String, backend | lib.Backend | default = "compose",
  hostSecretUid | Number, hostSecretGid | Number,
  passwordInput | String,
  postgresDb | String | default = "fixture",
  secrets = { password | lib.SecretSource },
} in
let env | cfg = import "env.yaml" in
let dc = lib.forEnvironment env in
dc.ComposeFile {
  dockstride | not_exported = {
    Config = cfg,
    setup = {
      secrets.password = lib.FileSecret env.passwordInput,
      secretAccess.password = { uid = env.hostSecretUid, gid = env.hostSecretGid },
    },
    commands.postgresDiagnosis = { argv = ["python3", "diagnose.py"], timeoutSeconds = 8 },
    diagnostics.postgres = { command = "postgresDiagnosis", services = ["postgres"],
      on = ["unhealthy", "startup-failed"] },
  },
  secrets = env.secrets,
  volumes.data = {},
  services.postgres = dc.Service {
    image = "%{env.project}-postgres:smoke",
    user = "0:0",
    build = "./postgres",
    environment = {
      POSTGRES_USER = "fixture", POSTGRES_DB = env.postgresDb,
      POSTGRES_PASSWORD_FILE = "/run/secrets/password",
      POSTGRES_HOST_AUTH_METHOD = "scram-sha-256",
    },
    secrets = [{ source = "password", target = "password" }],
    volumes = ["data:/var/lib/postgresql/data"],
    healthcheck = {
      test = ["CMD-SHELL", "PGPASSWORD=$$(cat /run/secrets/password) PGCONNECT_TIMEOUT=2 psql -X --no-password -h $$(hostname -i | cut -d ' ' -f 1) -U fixture -d $$POSTGRES_DB -qAt -c 'SELECT 1' >/dev/null 2>&1"],
      interval = "1s", timeout = "3s", start_period = "60s", retries = 2,
    },
  },
}
'''
        self.compose_private_file(project / "compose.ncl", model)
        self.compose_private_file(project / "postgres" / "Dockerfile",
                                  "FROM postgres:17\nCOPY --chown=postgres:postgres 01-hold.sh 02-marker.sql /docker-entrypoint-initdb.d/\n"
                                  "RUN chmod 0644 /docker-entrypoint-initdb.d/*\n")
        # A real cold database stays in initialization while the bounded up
        # fails. The temporary socket server is not the network-ready service.
        self.compose_private_file(project / "postgres" / "01-hold.sh", "sleep 45\n")
        self.compose_private_file(project / "postgres" / "02-marker.sql",
                                  "CREATE TABLE smoke_sentinel(value text NOT NULL);\nINSERT INTO smoke_sentinel VALUES ('preserve-me');\n")
        probe = r'''import ipaddress
import json
import re
import subprocess
import sys
import time

PROBE = r"""
export LC_ALL=C
unset PGHOSTADDR PGSERVICE PGSERVICEFILE PGOPTIONS PGPASSWORD
export PGHOST="$1" PGPORT=5432 PGUSER=fixture PGDATABASE="$2"
export PGCONNECT_TIMEOUT=2 PGPASSFILE=/dev/null
export PGOPTIONS='-c default_transaction_read_only=on -c statement_timeout=1500'
pg_isready -q -h "$PGHOST" -p 5432 -t 1 >/dev/null 2>&1 || {
    printf '%s\n' not-ready; exit 0;
}
[ "${POSTGRES_PASSWORD_FILE:-}" = /run/secrets/password ] &&
[ -r /run/secrets/password ] || { printf '%s\n' credential-unavailable; exit 0; }
password=$(cat /run/secrets/password 2>/dev/null)
[ -n "$password" ] || { printf '%s\n' credential-unavailable; exit 0; }
export PGPASSWORD="$password"
unset password
result=$(psql -X --no-password -qAt -c 'SELECT 1' 2>&1)
status=$?
unset PGPASSWORD
if [ "$status" -eq 0 ] && [ "$result" = 1 ]; then
    printf '%s\n' healthy
else
    case "$result" in
        *'password authentication failed'*|*'no password supplied'*) printf '%s\n' authentication-failed ;;
        *'database "'*'" does not exist'*) printf '%s\n' missing-database ;;
        *'the database system is starting up'*) printf '%s\n' not-ready ;;
        *) printf '%s\n' connection-unavailable ;;
    esac
fi
"""
deadline = time.monotonic() + 7
def docker(*args):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError()
    return subprocess.run(["docker", *args], stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                          text=True, timeout=min(3.5, remaining))

context = json.load(sys.stdin)
rows = context.get("diagnostics", {}).get("observations", {}).get("services", [])
rows = [row for row in rows if row.get("service") == "postgres"]
ids = rows[0].get("verifiedContainerIds", []) if len(rows) == 1 else []
code = "unverified-resource"
evidence = {"service": "postgres", "readOnly": True}
try:
    if len(ids) == 1 and re.fullmatch(r"[0-9a-f]{64}", ids[0]):
        inspected = docker("inspect", "--type", "container", ids[0])
        if inspected.returncode == 0:
            item = json.loads(inspected.stdout)[0]
            if item["Id"] == ids[0]:
                evidence["verifiedContainerId"] = ids[0]
                code = "not-ready"
                if item["State"]["Running"]:
                    networks = item["NetworkSettings"]["Networks"]
                    addresses = sorted({str(ipaddress.ip_address(row["IPAddress"])) for row in networks.values()
                                        if row.get("IPAddress")})
                    mounts = [row for row in item["Mounts"] if row["Destination"] == "/run/secrets/password"]
                    if len(addresses) == 1 and len(mounts) == 1 and not mounts[0]["RW"]:
                        evidence["networkAddress"] = addresses[0]
                        database = context.get("settings", {}).get("postgresDb", "fixture")
                        result = docker("exec", ids[0], "sh", "-c", PROBE, "diagnostic",
                                        addresses[0], database)
                        allowed = {"healthy", "not-ready", "authentication-failed", "missing-database",
                                   "credential-unavailable", "connection-unavailable"}
                        candidate = result.stdout.strip()
                        code = candidate if result.returncode == 0 and candidate in allowed else "connection-unavailable"
except (OSError, ValueError, KeyError, TimeoutError, subprocess.TimeoutExpired):
    code = "connection-unavailable"
summaries = {
    "healthy": "Read-only PostgreSQL network authentication and SELECT 1 succeeded.",
    "not-ready": "PostgreSQL is still initializing or not accepting network connections.",
    "missing-database": "The configured database is absent from the initialized volume.",
    "authentication-failed": "The mounted credential does not authenticate against the initialized volume.",
}
finding = {"code": "postgres." + code, "severity": "info" if code == "healthy" else "error",
           "summary": summaries.get(code, "A verified read-only connection could not be established."),
           "evidence": evidence}
if code == "authentication-failed":
    finding["suggestedAction"] = "Explicitly restore the credential matching this volume, or consciously choose a destructive reset; importing a new password does not change database roles."
if code == "missing-database":
    finding["suggestedAction"] = "Choose the existing database setting, or explicitly provision the intended database."
json.dump({"schemaVersion": 1, "findings": [finding]}, sys.stdout)
sys.stdout.write("\n")
'''
        self.compose_private_file(project / "diagnose.py", probe)
        inputs = {"project": name, "hostSecretUid": os.getuid(), "hostSecretGid": os.getgid(),
                  "passwordInput": str(password)}
        argv = ["setup"]
        for field, value in inputs.items():
            argv.extend(["--set", field + "=" + json.dumps(value)])
        self.cli(project, *argv)
        # Remove image pull/build time from the deliberately short cold-start
        # deadline; the lifecycle failure must observe PostgreSQL initializing.
        self.cli(project, "compose", "build", "postgres", timeout=900, lifecycle_timeout=900)
        error = self.compose_bounded_cli(project, 8, "up", fail=True)
        self.compose_postgres_finding(project, error["details"]["diagnostics"], "not-ready")
        require(error["category"] == "operation" and error["exitCode"] == 1 and
                any(word in error["message"].lower() for word in ("health", "deadline")),
                "initialization diagnostic replaced primary startup failure")
        cid = self.json_docker("inspect", self.compose_container(name, "postgres"))[0]["Id"]
        poll(lambda: self.json_docker("inspect", cid)[0]["State"]["Health"]["Status"],
             lambda value: value == "healthy", "initialized PostgreSQL fixture", timeout=75)
        self.cli(project, "up", timeout=120, lifecycle_timeout=90)
        doctor = self.cli(project, "doctor")["diagnostics"]
        self.compose_postgres_finding(project, doctor, "healthy")
        volumes = self.json_docker("inspect", cid)[0]["Mounts"]
        data_volume = next(row["Name"] for row in volumes if row["Destination"] == "/var/lib/postgresql/data")
        self.config(project, "postgresDb", "deliberately_absent_fixture_db")
        error = self.compose_bounded_cli(project, 15, "up", fail=True)
        self.compose_postgres_finding(project, error["details"]["diagnostics"], "missing-database")
        require(error["category"] == "operation" and error["exitCode"] == 1 and
                any(word in error["message"].lower() for word in ("health", "deadline", "postgres")),
                "missing-database finding replaced original startup failure")
        self.config(project, "postgresDb", "fixture")
        self.cli(project, "down")
        wrong = self.root / "postgres-wrong.input"
        self.compose_private_file(wrong, uuid.uuid4().hex + "\n")
        self.cli(project, "secrets", "replace", "password", "--file", str(wrong))
        error = self.compose_bounded_cli(project, 15, "up", fail=True)
        finding = self.compose_postgres_finding(project, error["details"]["diagnostics"], "authentication-failed")
        require(finding.get("suggestedAction") and error["category"] == "operation" and error["exitCode"] == 1,
                "credential mismatch did not retain primary failure and inert recovery guidance")
        require(self.cli(project, "config", "get", "passwordInput")["value"] == str(password),
                "diagnosis changed credential provider source")
        # Restoring the correct mounted credential, not ALTER ROLE or volume
        # reset, must recover the original data. A destructive hook cannot pass.
        self.cli(project, "down")
        self.cli(project, "secrets", "replace", "password", "--file", str(password))
        self.cli(project, "up", timeout=120, lifecycle_timeout=90)
        current = self.json_docker("inspect", self.compose_container(name, "postgres"))[0]
        require(next(row["Name"] for row in current["Mounts"] if row["Destination"] == "/var/lib/postgresql/data") == data_volume,
                "diagnostics replaced initialized PostgreSQL volume")
        self.compose_postgres_finding(project, self.cli(project, "doctor")["diagnostics"], "healthy")
        # The password remains entirely inside the container's environment.
        marker = self.docker("exec", current["Id"], "sh", "-c",
                             "PGPASSWORD=$(cat /run/secrets/password) PGCONNECT_TIMEOUT=2 "
                             "psql -X --no-password -h \"$(hostname -i | cut -d ' ' -f 1)\" -U fixture -d fixture -qAt "
                             "-c 'SELECT value FROM smoke_sentinel'")
        require(marker.stdout.strip() == "preserve-me", "read-only diagnostics altered PostgreSQL data or credentials")
        self.report.append("PostgreSQL: real initializing/missing-database/initialized-volume credential mismatch; verified own-network read-only probes, original startup failures retained, inert recovery suggestions, correct credential restores sentinel without role/volume mutation")


    def compose_postgres_finding(self, project, report, expected):
        require(not report["failures"], "PostgreSQL project diagnostic hook failed")
        matches = [row for row in report["findings"] if row["code"] == "postgres." + expected]
        require(len(matches) == 1, "PostgreSQL diagnostic did not classify " + expected)
        cid = self.json_docker("inspect", self.compose_container(
            self.cli(project, "config", "get", "project")["value"], "postgres"))[0]["Id"]
        require(matches[0]["evidence"].get("verifiedContainerId") == cid and
                matches[0]["evidence"].get("readOnly") is True,
                "PostgreSQL diagnostic did not probe the verified fixture container")
        return matches[0]


    def swarm_private_write(self, path, content):
        # All newly created fixture inputs are current-user, private regular files.
        path = Path(path)
        require(not path.is_symlink(), "Swarm fixture input must not be a symlink")
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as output:
            os.fchmod(output.fileno(), 0o600)
            output.write(content.encode() if isinstance(content, str) else content)
        metadata = path.stat()
        require(metadata.st_uid == os.getuid() and metadata.st_mode & 0o777 == 0o600,
                "Swarm fixture input is not current-user mode0600")


    def swarm_service(self, name, service):
        item = self.json_docker("service", "inspect", name + "_" + service)[0]
        labels = item["Spec"].get("Labels", {})
        require(labels.get("io.dockstride.project") == name and labels.get("io.dockstride.owner"),
                "refusing to manipulate a non-owned Swarm service")
        return item


    def swarm_secret_ids(self, name, owner):
        return set(self.docker("secret", "ls", "-q", "--filter", "label=io.dockstride.owner=" + owner,
                               "--filter", "label=io.dockstride.project=" + name).stdout.split())


    def swarm_secret(self, project, name, reference, owner=None):
        require(reference.get("external") is True and isinstance(reference.get("name"), str),
                "Swarm credential is not an immutable external reference")
        item = self.json_docker("secret", "inspect", reference["name"])[0]
        labels = item["Spec"].get("Labels", {})
        current_owner = json.loads((project / ".dockstride" / "identity.json").read_text())["id"]
        require(labels.get("io.dockstride.owner") == current_owner and
                (owner is None or current_owner == owner) and
                labels.get("io.dockstride.project") == name and
                labels.get("io.dockstride.secret") == "authKey" and
                labels.get("io.dockstride.revision") and item.get("ID"),
                "Swarm revision lacks verified immutable identity/ownership labels")
        return item


    def swarm_status_row(self, report, service, failed=False):
        require(report.get("backend") == "swarm" and isinstance(report.get("services"), list) and
                service in report.get("requiredServices", []), "incomplete Swarm status report")
        require(not failed or report.get("ready") is False, "failed status claims aggregate readiness")
        rows = [row for row in report["services"] if row.get("name") == service]
        require(len(rows) == 1 and rows[0].get("required") is True,
                "selected Swarm service missing from complete status report")
        return rows[0]


    def swarm_failed_status(self, project, service):
        error = self.cli(project, "status", service, fail=True)
        report = error.get("details", {}).get("status")
        require(isinstance(report, dict), "failed strict status omitted details.status")
        return self.swarm_status_row(report, service, failed=True)


    def swarm_wait_tasks(self, name, service, replicas=1):
        native = name + "_" + service
        poll(lambda: self.docker("service", "ps", "--filter", "desired-state=running", "--format",
                                "{{.CurrentState}}", native).stdout.splitlines(),
             lambda rows: len(rows) == replicas and all(row.startswith("Running ") for row in rows),
             "restored owned Swarm task scope")


    def swarm_strict_status(self, project, name):
        report = self.cli(project, "status")
        api = self.swarm_status_row(report, "api")
        worker = self.swarm_status_row(report, "worker")
        require(report.get("ready") is True and api.get("containerReady") is True and
                api.get("applicationReady") is True and worker.get("containerReady") is True,
                "healthy Swarm status did not verify native and application readiness")
        inspected = self.cli(project, "status", "--inspect-only")
        require(inspected.get("inspectOnly") is True and
                all(row.get("applicationReady") is None for row in inspected["services"]),
                "inspect-only claimed application readiness")

        # A readiness-only model fault does not alter the healthy native service.
        model_path = project / "compose.ncl"
        model = model_path.read_text()
        needle = 'json = { application = "dockstride-sample", project = env.project, uid = env.containerUid }'
        require(model.count(needle) == 1, "sample readiness declaration changed")
        try:
            self.swarm_private_write(model_path, model.replace(needle, needle.replace(
                'application = "dockstride-sample"', 'application = "deliberately-wrong-identity"')))
            api = self.swarm_failed_status(project, "api")
            require(api.get("containerReady") is True and api.get("applicationReady") is False,
                    "wrong HTTP application identity did not fail strict status independently of native health")
            api = self.swarm_status_row(self.cli(project, "status", "api", "--inspect-only"), "api")
            require(api.get("applicationReady") is None, "inspect-only executed wrong-identity probe")
            url = 'url = "http://127.0.0.1:%{port}/health"'
            require(model.count(url) == 1, "sample HTTP readiness URL changed")
            self.swarm_private_write(model_path, model.replace(url, url.replace("/health", "/deliberate-404")))
            api = self.swarm_failed_status(project, "api")
            require(api.get("containerReady") is True and api.get("applicationReady") is False,
                    "HTTP application probe failure was not represented in status")
        finally:
            self.swarm_private_write(model_path, model)
        require(self.cli(project, "status", "api").get("ready") is True,
                "restored application check did not recover")

        item = self.swarm_service(name, "worker")
        self.docker("service", "rm", item["ID"])
        poll(lambda: self.docker("service", "ls", "-q", "--filter", "name=" + name + "_worker").stdout.strip(),
             lambda value: not value, "selected service removal")
        row = self.swarm_failed_status(project, "worker")
        require(row.get("status") == "missing" and row.get("containerReady") is False,
                "removed selected Swarm service did not fail as missing")
        self.cli(project, "deploy", "worker", timeout=900)
        self.swarm_wait_tasks(name, "worker")
        require(self.cli(project, "status", "worker").get("ready") is True,
                "selected service restore did not recover")

        item = self.swarm_service(name, "worker")
        self.docker("service", "scale", item["ID"] + "=0")
        poll(lambda: self.docker("service", "ps", "--filter", "desired-state=running", "-q",
                                item["ID"]).stdout.strip(), lambda value: not value, "controlled replica shortfall")
        try:
            row = self.swarm_failed_status(project, "worker")
            require(row.get("desiredReplicas") == 0 and row.get("expectedReplicas") == 1 and
                    row.get("successfulTasks") == 0 and row.get("containerReady") is False and
                    "replica" in row.get("error", "").lower(),
                    "replica shortfall did not preserve desired/applied/full status observations")
        finally:
            self.swarm_service(name, "worker")
            self.docker("service", "scale", item["ID"] + "=1")
        self.swarm_wait_tasks(name, "worker")
        require(self.cli(project, "status").get("ready") is True, "replica restoration did not recover")


    def swarm_sync_report(self, report, source, status, committed, applied):
        require(report.get("operation") == "secrets-sync" and report.get("committed") == committed and
                report.get("uncommitted") == [] and report.get("applied") == applied,
                "secret sync did not precisely report committed/uncommitted/applied names")
        rows = report.get("secrets", [])
        require(len(rows) == 1 and rows[0].get("name") == "authKey" and rows[0].get("status") == status and
                rows[0].get("source") == {"kind": "file", "canonicalPath": str(source.resolve()),
                                          "origin": "declared-file"},
                "secret sync lost current declared file provenance or comparison result")
        serialized = json.dumps(report)
        require("keyedDigest" not in serialized and "keyed_digest" not in serialized,
                "secret sync exposed a private keyed digest")
        return rows[0]


    def swarm_retire_fixture_snapshots(self, project, name, reference):
        # Explicit fixture manipulation, NOT a Dockstride snapshot-removal API.
        # Journals also retain plans and previous/completed deployment snapshots.
        # Retire whole reference-bearing journals, never rewrite recorded history.
        # Current env refs, revision history, identity, and private stores survive.
        root = self.root.resolve()
        require(project == project.resolve() and project.is_relative_to(root) and
                any(Path(path) == project and saved_name == name for path, saved_name, _ in self.projects),
                "snapshot retirement escaped the tracked disposable fixture")
        state_dir = project / ".dockstride"
        for directory in (project, state_dir):
            metadata = directory.lstat()
            require(stat.S_ISDIR(metadata.st_mode) and metadata.st_uid == os.getuid() and
                    not directory.is_symlink(), "unsafe fixture snapshot directory")
        require(state_dir.stat().st_mode & 0o777 == 0o700, "unsafe fixture state directory mode")

        observed = {}
        def version(metadata):
            return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_uid,
                    metadata.st_size, metadata.st_mtime_ns)

        def private_metadata(path):
            require(path.parent == state_dir, "snapshot path escaped fixture bookkeeping")
            metadata = path.lstat()
            require(stat.S_ISREG(metadata.st_mode) and metadata.st_uid == os.getuid() and
                    metadata.st_mode & 0o777 == 0o600, "unsafe fixture snapshot bookkeeping")
            observed[path] = version(metadata)
            return metadata

        def read_bookkeeping(path):
            metadata = private_metadata(path)
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd) as source:
                require(version(os.fstat(source.fileno())) == version(metadata),
                        "fixture snapshot changed while opening")
                return source.read()

        identity = json.loads(read_bookkeeping(state_dir / "identity.json"))
        owner = identity["id"]
        manager = self.dind_nodes["manager"]
        context = manager["context"] + ";" + manager["endpoint"]
        require(reference.get("external") is True and set(reference) == {"external", "name"} and
                isinstance(reference.get("name"), str), "unsafe fixture old Swarm reference")
        require(identity.get("root") == str(project) and identity.get("project") == name and
                identity.get("backend") == "swarm" and identity.get("context") == context and
                identity.get("secretCluster") == self.swarm_identity["cluster"] and
                self.env.get("DOCKER_CONTEXT") == manager["context"] and manager["context"] in self.contexts,
                "snapshot retirement ownership or pinned context mismatch")
        require(not self.docker("service", "ls", "-q", "--filter", "label=io.dockstride.owner=" + owner,
                                "--filter", "label=io.dockstride.project=" + name).stdout.strip(),
                "fixture snapshots cannot retire before its live service grants disappear")

        def contains(value):
            return value == reference or (isinstance(value, dict) and any(contains(v) for v in value.values())) or (
                isinstance(value, list) and any(contains(v) for v in value))

        def operation_id(value, prefix):
            suffix = value.removeprefix(prefix)
            require(value.startswith(prefix) and len(suffix) == 32 and
                    all(char in "0123456789abcdef" for char in suffix),
                    "unexpected fixture operation identity")
            return value

        def deployment_record(value):
            require(isinstance(value, dict) and value.get("owner") == owner and value.get("context") == context and
                    value.get("project") == name, "fixture deployment snapshot ownership mismatch")
            revision = operation_id(value["revision"], "deploy-")
            require(value.get("snapshot") == str(state_dir / (revision + ".yaml")),
                    "snapshot YAML path escaped fixture bookkeeping")

        def journal(path, prefix):
            operation = operation_id(path.name.removeprefix("journal-").removesuffix(".jsonl"), prefix)
            rows = [json.loads(line) for line in read_bookkeeping(path).splitlines()]
            require(rows and all(row.get("operation") == operation and isinstance(row.get("event"), dict) and
                                isinstance(row.get("timestamp_ms"), int) for row in rows),
                    "unsafe fixture operation journal")
            return rows

        # Historical deployment records say active=true at their creation time.
        # A later completed native down plus live-grant absence proves inactivity.
        down_completed = []
        for path in state_dir.glob("journal-down-*.jsonl"):
            rows = journal(path, "down-")
            plan = rows[0]["event"].get("plan", {})
            require(plan.get("backend") == "swarm" and plan.get("project") == name and
                    plan.get("context") == context, "fixture down journal context mismatch")
            if rows[-1]["event"].get("state") == "completed":
                down_completed.append(rows[-1]["timestamp_ms"])
        require(down_completed, "fixture has no completed native down before snapshot retirement")
        inactive_since = max(down_completed)
        retired = []
        removals = set()
        candidates = [state_dir / "deployment.json", state_dir / "deployment-applied.json"]
        candidates.extend(state_dir.glob("snapshot-deploy-*.json"))
        for path in candidates:
            if not path.exists() and not path.is_symlink():
                continue
            value = json.loads(read_bookkeeping(path))
            require(value.get("owner") == owner and value.get("context") == context,
                    "snapshot retirement ownership or pinned context mismatch")
            if path.name in ("deployment.json", "deployment-applied.json"):
                deployment_record(value)
                require(value.get("active") is False, "fixture deployment must be inactive before snapshot retirement")
                revision = value["revision"]
            else:
                revision = operation_id(path.name.removeprefix("snapshot-").removesuffix(".json"), "deploy-")
            if not contains(value):
                continue
            rows = journal(state_dir / ("journal-" + revision + ".jsonl"), "deploy-")
            require(rows[-1]["event"].get("state") in ("completed", "failed", "cancelled") and
                    max(row["timestamp_ms"] for row in rows) <= inactive_since,
                    "fixture snapshot operation is not proven inactive")
            yaml_path = state_dir / (revision + ".yaml")
            if yaml_path.exists() or yaml_path.is_symlink():
                private_metadata(yaml_path)
                removals.add(yaml_path)
            removals.add(path)
            retired.append(path.name)

        for path in state_dir.glob("journal-deploy-*.jsonl"):
            rows = journal(path, "deploy-")
            if not contains(rows):
                continue
            require(rows[-1]["event"].get("state") in ("completed", "failed", "cancelled") and
                    max(row["timestamp_ms"] for row in rows) <= inactive_since,
                    "fixture deployment operation is not proven inactive")
            plan = rows[0]["event"].get("plan", {})
            require(plan.get("backend") == "swarm" and plan.get("project") == name and
                    plan.get("context") == context, "fixture deploy journal context mismatch")
            resources = plan.get("sharedResources", {})
            labels = [resource.get("labels", {}) for kind in ("networks", "volumes")
                      for resource in (resources.get(kind) or {}).values()]
            require(labels and all(label.get("io.dockstride.owner") == owner and
                                   label.get("io.dockstride.project") == name for label in labels),
                    "fixture deploy plan ownership mismatch")
            for row in rows:
                event = row["event"]
                if "plan" in event:
                    require(event["plan"] == plan, "unexpected fixture deployment plan")
                for field in ("previous", "deployment"):
                    record = event.get(field)
                    if record:
                        deployment_record(record)
            removals.add(path)
            retired.append(path.name)

        require(retired, "fixture had no retained snapshots to retire")
        # Validate everything before the first mutation; never enumerate credential
        # stores or delete wildcard JSON files or unrelated operation journals.
        require(all(version(path.lstat()) == saved for path, saved in observed.items()),
                "fixture snapshot bookkeeping changed before retirement")
        for path in sorted(removals):
            path.unlink()
        self.report.append("fixture-only retirement of verified owned inactive Swarm deployment and journal snapshot references (not a public command)")


    def swarm_import_sync_smoke(self):
        project = self.checkout("swarm-file")
        source = self.root / "swarm-import-source"
        source_values = [uuid.uuid4().hex + uuid.uuid4().hex for _ in range(3)]
        self.swarm_private_write(source, source_values[0])
        model_path = project / "compose.ncl"
        model = model_path.read_text()
        policy = 'lib.GenerateSecret { bytes = 32, encoding = "hex", durable = true, recoveryFile = env.recoveryFile }'
        require(model.count(policy) == 1, "sample generated-secret policy changed")
        model = model.replace(policy, "lib.FileSecret " + json.dumps(str(source.resolve())))
        self.swarm_private_write(model_path, model)
        name = self.setup(project, "swarm-file", "swarm", self.app_port)
        first = self.secret_ref(project)
        first_object = self.swarm_secret(project, name, first)
        owner = first_object["Spec"]["Labels"]["io.dockstride.owner"]
        plan = self.cli(project, "secrets", "sync", "authKey", "--plan")
        require(plan.get("comparisonsDeferred") is True and plan.get("sideEffects") is False and
                plan.get("committed") == [] and plan.get("applied") == [] and plan.get("uncommitted") == ["authKey"],
                "sync plan claimed a comparison/publication")
        require(plan.get("secrets") == [{"name": "authKey", "source": {
                    "kind": "file", "canonicalPath": str(source.resolve()), "origin": "declared-file"},
                    "status": "comparison-deferred", "reference": first}], "sync plan lost deferred source provenance")
        initial_ids = self.swarm_secret_ids(name, owner)
        require(initial_ids == {first_object["ID"]}, "initial file import created extra owned revisions")
        unchanged = self.cli(project, "secrets", "sync", "authKey", "--yes")
        self.swarm_sync_report(unchanged, source, "unchanged", [], [])
        require(self.secret_ref(project) == first and self.swarm_secret(project, name, first)["ID"] == first_object["ID"],
                "comparable initial imported Swarm secret was not an unchanged no-op")
        require(self.swarm_secret_ids(name, owner) == initial_ids,
                "unchanged file sync created an orphan immutable revision")
        self.cli(project, "deploy", timeout=900)
        identity = self.identity(self.app_port, name)
        self.swarm_private_write(source, source_values[1])
        self.cli(project, "deploy", timeout=900)
        require(self.secret_ref(project) == first and
                self.identity(self.app_port, name)["secretFingerprint"] == identity["secretFingerprint"],
                "ordinary deployment implicitly synchronized changed file input")
        rejected = self.cli(project, "secrets", "sync", "authKey", "--yes", "--apply", fail=True)
        require("rotation" in rejected.get("message", "").lower() and self.secret_ref(project) == first and
                self.swarm_secret(project, name, first, owner)["ID"] == first_object["ID"],
                "undeclared application rotation was not rejected before reference publication")
        require(self.swarm_secret_ids(name, owner) == initial_ids,
                "undeclared apply created a backend object before preflight rejection")
        changed = self.cli(project, "secrets", "sync", "authKey", "--yes")
        row = self.swarm_sync_report(changed, source, "replaced", ["authKey"], [])
        second = self.secret_ref(project)
        second_object = self.swarm_secret(project, name, second, owner)
        require(first != second and first_object["ID"] != second_object["ID"] and
                first_object["Spec"]["Labels"]["io.dockstride.revision"] !=
                second_object["Spec"]["Labels"]["io.dockstride.revision"] and
                row.get("previous") == first and row.get("reference") == second and
                row.get("priorContentComparable") is True and row.get("consumerRestartNeeded") is True,
                "changed source did not publish a distinct verified immutable owned revision")
        self.swarm_secret(project, name, first, owner)
        self.swarm_sync_report(self.cli(project, "secrets", "sync", "authKey", "--yes"), source, "unchanged", [], [])
        for service in ("api", "worker"):
            grants = self.swarm_service(name, service)["Spec"]["TaskTemplate"]["ContainerSpec"]["Secrets"]
            require(any(grant.get("SecretID") == first_object["ID"] for grant in grants),
                    "storage-only sync unexpectedly changed live service grants")
        blocked = self.docker("secret", "rm", first_object["ID"], ok=False)
        require(blocked.returncode != 0 and "in use" in (blocked.stderr + blocked.stdout).lower(),
                "live Swarm grant did not block old immutable object removal")
        retained = self.cli(project, "secrets", "gc", first["name"], "--plan")["plan"]
        require(len(retained) == 1 and retained[0].get("eligible") is False and
                retained[0].get("reason") == "retained deployment snapshot", "old imported revision lost snapshot protection")
        self.cli(project, "secrets", "gc", first["name"], "--yes", fail=True)

        # Only this disposable model receives an explicit, matching command rotation.
        marker = project / "rotation-command-ran"
        rotation = ('{ name = "deliberate-rotation-failure", kind = "command", '
                    'argv = ["python3", "-c", ' + json.dumps(
                        "from pathlib import Path; import os; p=Path(" + repr(str(marker)) +
                        "); fd=os.open(p, os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600); "
                        "os.close(fd); raise SystemExit(23)") +
                    '], workflows = ["fixture-rotate-auth"], stage = "before", services = ["api", "worker"] }')
        rotating_model = model.replace("] else [],\n    oneshots =",
                                       "] else [" + rotation + "],\n    oneshots =", 1)
        require(rotating_model.count("      secretAccess.authKey =") == 1,
                "sample setup metadata changed before fixture rotation declaration")
        rotating_model = rotating_model.replace("      secretAccess.authKey =", 
            '      rotations.authKey = { workflow = "fixture-rotate-auth", services = ["api", "worker"] },\n'
            "      secretAccess.authKey =")
        self.swarm_private_write(model_path, rotating_model)
        self.swarm_private_write(source, source_values[2])
        failed = self.cli(project, "secrets", "sync", "authKey", "--yes", "--apply", fail=True)
        report = failed.get("details", {}).get("secretSync")
        require(isinstance(report, dict), "failed apply omitted precise details.secretSync")
        row = self.swarm_sync_report(report, source, "replaced", ["authKey"], [])
        third = self.secret_ref(project)
        third_object = self.swarm_secret(project, name, third, owner)
        require(third not in (first, second) and third_object["ID"] not in (first_object["ID"], second_object["ID"]) and
                row.get("previous") == second and row.get("reference") == third and row.get("applied") is False and
                row.get("consumerRestartNeeded") is True and marker.is_file() and
                marker.stat().st_uid == os.getuid() and marker.stat().st_mode & 0o777 == 0o600,
                "failed local rotation did not retain committed storage and prove command execution")
        for reference in (first, second, third):
            self.swarm_secret(project, name, reference, owner)
        require(self.identity(self.app_port, name)["secretFingerprint"] == identity["secretFingerprint"],
                "failed inert rotation changed the live consumer credential")
        serialized = json.dumps((plan, unchanged, rejected, changed, failed))
        require(all(value not in serialized for value in source_values) and "keyedDigest" not in serialized and
                "keyed_digest" not in serialized, "Swarm sync/apply output leaked credential bytes/digests")
        self.swarm_private_write(model_path, model)
        self.cli(project, "down")
        require(self.secret_ref(project) == third, "Swarm down changed committed secret reference")
        retained = self.cli(project, "secrets", "gc", first["name"], "--plan")["plan"]
        require(len(retained) == 1 and retained[0].get("eligible") is False and
                retained[0].get("reason") == "retained deployment snapshot",
                "down silently discarded old revision snapshot protection")
        self.swarm_retire_fixture_snapshots(project, name, first)
        require(self.secret_ref(project) == third and marker.is_file(),
                "fixture snapshot retirement changed the committed reference or rotation evidence")
        eligible = self.cli(project, "secrets", "gc", first["name"], "--plan")["plan"]
        require(len(eligible) == 1 and eligible[0].get("eligible") is True,
                "old imported revision stayed blocked after live grants and fixture snapshots cleared")
        self.cli(project, "secrets", "gc", first["name"], "--yes")
        require(self.docker("secret", "inspect", first_object["ID"], ok=False).returncode != 0,
                "eligible old revision was not explicitly garbage-collected")
        self.swarm_secret(project, name, second, owner)
        self.swarm_secret(project, name, third, owner)
        require(self.secret_ref(project) == third,
                "old revision garbage collection changed the committed current reference")
        self.cli(project, "destroy", "--yes")
        self.report.append("Swarm private FileSecret import/provenance/deferred plan, immutable unchanged/changed sync, ordinary deploy reuse, prepublication undeclared apply refusal, committed failed command apply with retained objects, live grants and retained snapshot GC protection")





def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dks", default="target/debug/dks", help="already-built CLI; this harness never builds Rust")
    parser.add_argument("--compose", action="store_true", help="exercise Compose on the selected daemon with UUID-owned resources")
    parser.add_argument("--swarm", action="store_true", help="create isolated DIND manager+worker+registry; NEVER initialize existing daemon")
    parser.add_argument("--timeout", type=int, default=90, help="Dockstride lifecycle deadline in seconds, including builds and rollout")
    parser.add_argument("--dind-image", default="docker:dind")
    parser.add_argument("--registry-image", default="registry:2")
    args = parser.parse_args()
    harness = None
    failure = None
    report = []
    temporary = Path(tempfile.mkdtemp(prefix="dockstride-smoke-"))
    try:
        harness = Harness(args, temporary)
        harness.config_smoke()
        if args.compose:
            harness.compose_smoke()
        if args.swarm:
            harness.swarm_smoke()
        report = harness.report
    except (SmokeFailure, OSError, ValueError, KeyError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        failure = str(error)
    finally:
        if harness:
            try:
                harness.cleanup()
            except (SmokeFailure, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
                failure = (failure + "\n" if failure else "") + str(error)
    if failure:
        print("SMOKE FAILED: " + failure, file=sys.stderr)
        print("Run-owned evidence/recovery retained at: " + str(temporary), file=sys.stderr)
        return 1
    shutil.rmtree(temporary)
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
