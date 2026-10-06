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
        self.outer_container_ids = {}
        self.node_envs = {}
        self.dind_nodes = {}
        self.swarm_identity = None
        self.host_daemon_id = None
        self.dind_data_paths = []
        self.contexts = []
        self.network = None
        self.network_id = None
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

    def reject_cli(self, project, *args):
        result = run([self.binary, "-C", project, *args], self.env, ok=False)
        require(result.returncode != 0, "removed CLI accepted: " + repr(args))

    def assert_disposable_state(self, project):
        forbidden = [path for path in (project / ".dockstride").rglob("*")
                     if path.suffix in (".json", ".jsonl", ".yaml", ".yml")]
        require(not forbidden, "durable runtime state created: " + repr(forbidden))
        for directory in (self.root / "private" / "dockstride",
                          self.root / "home" / ".local" / "share" / "dockstride",
                          self.root / "state" / "dockstride"):
            forbidden = [path for path in directory.rglob("*")
                         if path.suffix in (".json", ".jsonl") or path.name == ".dockstride-owner"]
            require(not forbidden, "hidden durable state created: " + repr(forbidden))

    def discard_scratch(self, project):
        self.assert_disposable_state(project)
        scratch = project / ".dockstride"
        if scratch.exists():
            require(not scratch.is_symlink() and project.resolve().is_relative_to(self.root.resolve()),
                    "scratch cleanup escaped fixture")
            shutil.rmtree(scratch)

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
            inputs.update(imagePrefix="registry:5000/" + name, containerGid=1000,
                          swarmDirectNetworking=True)
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

    def swarm_ref(self, project):
        return self.cli(project, "render", "--target", "swarm")["secrets"]["authKey"]

    def current_secret_listing(self, project, backend, reference, binding):
        rows = self.cli(project, "secrets", "list")["secrets"]
        require(len(rows) == 1 and set(rows[0]) ==
                {"name", "backend", "reference", "binding", "consumers", "present"} and
                rows[0]["name"] == "authKey" and rows[0]["backend"] == backend and
                rows[0]["reference"] == reference and rows[0]["binding"] == binding and
                set(rows[0]["consumers"]) == {"api", "worker"} and rows[0]["present"] is True,
                "current secret listing lost source/binding/consumer observations or exposed history")

    def config_smoke(self):
        # This incomplete checkout exercises configuration without a Docker daemon.
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
        inventory = self.cli(project, "env", "list")
        require(set(inventory) == {"schemaVersion", "status", "scope", "worktrees"} and
                inventory["schemaVersion"] == 1 and inventory["status"] == "available" and
                inventory["scope"] == "invoking-repository" and
                str(project.resolve()) in {row["root"] for row in inventory["worktrees"]},
                "Git worktree inventory omitted incomplete checkout")
        for args in (("ports", "release", "--yes"), ("ports", "gc", "--yes"),
                     ("env", "forget", str(project), "--yes"), ("env", "list", "--worktrees"),
                     ("secrets", "gc", "authKey", "--yes"), ("deploy", "api")):
            self.reject_cli(project, *args)
        self.assert_disposable_state(project)
        self.report.append("schema/incremental edits/comments/invalid candidate/no-effect plan; Git-only inventory and removed CLI rejection")
    

    def compose_smoke(self):
        self.host_daemon_id = self.docker("info", "--format", "{{.ID}}", env=self.host_env).stdout.strip()
        require(self.host_daemon_id, "selected Compose Docker daemon has no verifiable identity")
        first = self.checkout(self.prefix + "-compose a=λ")
        second = self.checkout(self.prefix + "-compose-b")
        for project in (first, second):
            self.compose_fixture_model(project)
        source = self.root / "private-import.input"
        self.compose_private_file(source, uuid.uuid4().hex.encode(), mode=0o640)
        name_a = self.compose_bootstrap(first, imported=source)
        shared = first / "env.shared.yaml"
        require(shared.is_file() and shared.stat().st_mode & 0o777 == 0o600,
                "defaults hook did not create private shared settings")
        # Stopped checkouts hold no reservation: a real bind keeps this endpoint
        # occupied while the second setup probes for its ordinary YAML value.
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", self.configured_port(first)))
            name_b = self.compose_bootstrap(second, source=shared)
        require(name_a != name_b, "distinct folder names produced the same project proposal")
        port_a, port_b = self.configured_port(first), self.configured_port(second)
        require(port_a != port_b, "allocation selected a currently bound endpoint")
        inventory = self.cli(first, "env", "list")
        require({str(first.resolve()), str(second.resolve())} <=
                {row["root"] for row in inventory["worktrees"]},
                "Git inventory omitted configured worktrees")
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
        self.current_secret_listing(first, "compose", refs[0], None)
        self.current_secret_listing(second, "compose", refs[1], None)
        generated_path = Path(refs[1]["file"])
        generated_bytes = generated_path.read_bytes()
        require(not generated_path.resolve().is_relative_to(second.resolve()),
                "generated credentials were stored inside the checkout")
        self.cli(second, "setup")
        require(self.secret_ref(second) == refs[1] and generated_path.read_bytes() == generated_bytes and
                self.configured_port(second) == port_b, "repeat setup changed materialized generated inputs")
        missing = generated_path.with_name(generated_path.name + ".smoke-held")
        generated_path.rename(missing)
        try:
            error = self.cli(second, "setup", fail=True)
            require(not generated_path.exists() and "restore" in error["message"].lower(),
                    "missing referenced generated bytes were regenerated or did not fail closed")
        finally:
            missing.rename(generated_path)
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
            blocked_yaml = (blocked / "env.yaml").read_bytes()
            error = self.cli(blocked, "up", fail=True, timeout=900)
            require("port" in error["message"].lower() or "address already in use" in error["message"].lower(),
                    "occupied-port startup did not explain conflict")
            require((blocked / "env.yaml").read_bytes() == blocked_yaml,
                    "occupied materialized endpoint was silently rerolled")
            require(listener.fileno() >= 0 and listener.getsockname()[1] == occupied,
                    "startup commandeered unrelated listener")
        count, migration = self.compose_once_up(first, name_a, port_a, 0)
        self.cli(second, "up", timeout=900)
        initial, other = self.identity(port_a, name_a), self.identity(port_b, name_b)
        require(initial["migrated"] and other["migrated"], "one-shot migration did not complete")
        require(initial["secretFingerprint"] != other["secretFingerprint"], "worktree secrets are not isolated")
        self.discard_scratch(first)
        report = self.cli(first, "status")
        api = next(row for row in report["services"] if row["name"] == "api")
        require(report["ready"] and api["applicationReady"] is True, "strict status did not verify application")
        owner = str(first.resolve())
        for service in ("api", "worker", "migrate"):
            labels = self.json_docker("inspect", self.compose_container(name_a, service))[0]["Config"]["Labels"]
            require(labels.get("io.dockstride.owner") == owner and
                    labels.get("io.dockstride.project") == name_a, "Compose labels lost canonical path ownership")
        alias = self.root / "canonical-alias"
        alias.symlink_to(first, target_is_directory=True)
        require(self.cli(alias, "status")["ready"], "symlink checkout did not retain canonical ownership")
        collision = self.checkout(self.prefix + "-collision")
        self.compose_fixture_model(collision)
        self.compose_private_file(collision / "env.yaml", (first / "env.yaml").read_bytes())
        original_ids = {service: self.json_docker("inspect", self.compose_container(name_a, service))[0]["Id"]
                        for service in ("api", "worker", "migrate")}
        for args in (("status",), ("up",), ("destroy", "--yes")):
            error = self.cli(collision, *args, fail=True)
            require(owner in error["message"], "collision error omitted observed foreign checkout")
        for service, container_id in original_ids.items():
            require(self.json_docker("inspect", self.compose_container(name_a, service))[0]["Id"] == container_id,
                    "foreign same-project checkout changed original resources")
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
        require(refs[0] == {"file": str(source.resolve())}, "original provider file was copied")
        original_bytes = source.read_bytes()
        self.cli(first, "setup")
        require(self.secret_ref(first) == refs[0] and source.read_bytes() == original_bytes and
                self.configured_port(first) == port_a,
                "repeated setup changed original reference, bytes, or ordinary port")
        deferred = self.cli(first, "secrets", "sync", "authKey", "--plan")
        require(deferred["sideEffects"] is False and deferred["secrets"][0]["status"] == "planned",
                "sync plan claimed validation/publication")
        self.compose_private_file(source, uuid.uuid4().hex.encode(), mode=0o640)
        synced = self.cli(first, "secrets", "sync", "authKey", "--yes")
        new_ref = self.secret_ref(first)
        require(synced["committed"] == [] and synced["uncommitted"] == [] and
                synced["secrets"][0]["status"] == "validated" and new_ref == refs[0] and
                synced["secrets"][0]["binding"] is None,
                "Compose sync copied or fictitiously committed provider bytes")
        require(synced["secrets"][0]["source"]["canonicalPath"] == str(source.resolve()),
                "sync changed source origin")
        self.cli(first, "down")
        self.discard_scratch(first)
        self.cli(first, "up", timeout=900)
        require(self.identity(port_a, name_a)["secretFingerprint"] != initial["secretFingerprint"],
                "restart did not consume changed original provider file")
        self.compose_postgres_smoke()
        other_ids = {service: self.json_docker("inspect", self.compose_container(name_b, service))[0]["Id"]
                     for service in ("api", "worker", "migrate")}
        other_yaml = (second / "env.yaml").read_bytes()
        self.cli(first, "destroy", "--plan")
        self.cli(first, "destroy", "--yes")
        for volume in volumes:
            require(self.docker("volume", "inspect", volume, ok=False).returncode != 0, "destroy preserved data volume")
        require(self.secret_ref(first) == new_ref, "destroy removed immutable credential")
        retained_yaml = (first / "env.yaml").read_bytes()
        self.discard_scratch(first)
        require((first / "env.yaml").read_bytes() == retained_yaml and
                self.configured_port(first) == port_a,
                "destroy or scratch removal changed ordinary YAML values")
        self.cli(first, "setup")
        self.cli(first, "up", timeout=900)
        self.identity(self.configured_port(first), name_a)
        require(self.secret_ref(first) == new_ref, "recreated fixture regenerated retained credential")
        for service, expected in other_ids.items():
            require(self.json_docker("inspect", self.compose_container(name_b, service))[0]["Id"] == expected,
                    "destroy/recreate altered another checkout")
        require((second / "env.yaml").read_bytes() == other_yaml,
                "fixture teardown changed another checkout's settings")
        require(self.identity(port_b, name_b)["secretFingerprint"] == other["secretFingerprint"],
                "recreate affected another checkout's credential")
        self.report.append("Compose: defaults/shared-source worktrees, ordinary bound-port allocation and overrides; strict readiness; fresh prerequisite execution, failure/cancellation/seed safety; non-root secrets/watch/logs/exec/data; direct-reference validation; scratch-independent isolated teardown")
    

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
        observed = self.docker("info", "--format", "{{.ID}}", env=self.host_env).stdout.strip()
        require(observed and (self.host_daemon_id is None or observed == self.host_daemon_id),
                "outer Docker target changed before isolated DIND setup")
        self.host_daemon_id = observed
        self.network = self.prefix + "-network"
        self.network_id = self.docker("network", "create", "--label", "dockstride.smoke=" + self.prefix,
                                      self.network, env=self.host_env).stdout.strip()
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
        for path, name, environment in self.projects:
            if environment == self.host_env:
                absent = self.cli(path, "status", "--inspect-only")
                require(absent["ready"] is False and absent["ownershipVerified"] and
                        absent["owner"] == str(path.resolve()) and absent["project"] == name and
                        absent["context"].startswith(self.env["DOCKER_CONTEXT"] + ";") and
                        all(not row["containers"] for row in absent["services"]),
                        "target switch reused host resources instead of current live observations")
                returned = self.cli(path, "status", "--inspect-only", env=environment)
                require(returned["ownershipVerified"] and returned["owner"] == str(path.resolve()) and
                        returned["project"] == name and returned["context"] != absent["context"] and
                        any(row["containers"] for row in returned["services"]) and
                        all(row["containerReady"] for row in returned["services"] if row["required"]),
                        "returning to selected host target lost live owned resources")
                break
        project = self.checkout("swarm")
        name = self.setup(project, "swarm", "swarm", self.app_port)
        secret_before = self.swarm_ref(project)
        self.swarm_secret(project, name, secret_before)
        source_file = Path(self.secret_ref(project)["file"])
        metadata = source_file.stat()
        require(source_file.is_file() and not source_file.is_symlink() and
                not source_file.resolve().is_relative_to(project.resolve()) and
                metadata.st_uid == os.getuid() and metadata.st_mode & 0o777 in (0o600, 0o640) and metadata.st_size > 0,
                "generated Swarm source is not a private external file")
        before_plan = (project / "env.yaml").read_bytes()
        self.cli(project, "deploy", "--plan")
        require((project / "env.yaml").read_bytes() == before_plan and self.swarm_ref(project) == secret_before,
                "Swarm plan mutated environment or secret")
        self.cli(project, "deploy", timeout=900)
        identity = self.identity(self.app_port, name)
        require(len(self.docker("node", "ls", "-q").stdout.split()) == 1, "fixture was not single-node")
        for service in ("api", "worker"):
            require("@sha256:" in self.swarm_service(name, service)["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"],
                    "single-node Swarm did not deploy immutable image digests")
        self.cli(project, "deploy", timeout=900)
        require(self.swarm_ref(project) == secret_before and
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
        require(self.swarm_ref(project) == secret_before, "two-node deployment regenerated secret")
        previous_image = self.swarm_service(name, "api")["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"]
        previous_revision = identity["revision"]
        revision = "full-update-" + uuid.uuid4().hex[:8]
        self.swarm_private_write(project / "api" / "content" / "revision.txt", revision + "\n")
        deployed = self.cli(project, "deploy", timeout=900)
        require(set(deployed) == {"deployed", "context", "services", "convergence"} and
                set(deployed["services"]) == {"api", "worker"},
                "full deployment omitted authored services or exposed historical state")
        updated_identity = self.identity(self.app_port, name, revision)
        require(self.swarm_ref(project) == secret_before and
                updated_identity["secretFingerprint"] == identity["secretFingerprint"],
                "full deployment regenerated application credential")
        after = self.swarm_service(name, "worker")
        require(after["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"] !=
                worker_spec["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"],
                "full deploy did not update worker to newly built image")
        self.docker("service", "update", "--rollback", name + "_api")
        self.swarm_wait_tasks(name, "api")
        self.identity(self.app_port, name, previous_revision)
        require(self.swarm_service(name, "api")["Spec"]["TaskTemplate"]["ContainerSpec"]["Image"] == previous_image,
                "Docker-native rollback did not preserve prior immutable digest")
        self.cli(project, "deploy", timeout=900)
        self.identity(self.app_port, name, revision)
        self.discard_scratch(project)
        self.swarm_strict_status(project, name)
        self.cli(project, "logs", "--tail", "20", "api")
        unsupported = self.cli(project, "exec", "api", "true", fail=True)
        require("compose" in unsupported.get("message", "").lower(), "Swarm exec lost its node-local task boundary")
        api_id = poll(lambda: self.docker("ps", "-q", "--filter", "label=com.docker.swarm.service.name=" + name + "_api").stdout.strip(),
                      bool, "owned manager API task")
        self.docker("exec", api_id, "python", "-c", "from pathlib import Path; Path('/data/smoke-preserved').write_text('retained')")
        self.config(project, "failHealth", True)
        error = self.cli(project, "deploy", fail=True, timeout=900)
        require(any(word in error["message"].lower() for word in ("rollout", "task", "health", "converge", "pause")),
                "failed rollout lacks actionable task/health/convergence error")
        row = self.swarm_failed_status(project, "api")
        require(row.get("containerReady") is False and row.get("ready") is False,
                "failed rollout did not fail strict status")
        self.config(project, "failHealth", False)
        self.cli(project, "deploy", timeout=900)
        self.identity(self.app_port, name, revision)
        require(self.cli(project, "status").get("ready") is True, "failed rollout restoration did not recover")
        api_id = poll(lambda: self.docker("ps", "-q", "--filter", "label=com.docker.swarm.service.name=" + name + "_api").stdout.strip(),
                      bool, "restored manager API task")
        self.docker("exec", api_id, "python", "-c", "from pathlib import Path; assert Path('/data/smoke-preserved').read_text() == 'retained'")
        self.cli(project, "down")
        require(self.swarm_ref(project) == secret_before and self.configured_port(project) == self.app_port,
                "Swarm down changed stable credential/endpoint")
        self.cli(project, "status", fail=True)
        refused = self.cli(project, "destroy", "--yes", fail=True)
        require("multi-node" in refused.get("message", "").lower(),
                "multi-node destroy did not preserve conservative node-local volume boundary")
        self.retire_swarm_worker()
        self.cli(project, "destroy", "--yes")
        require(self.swarm_ref(project) == secret_before and source_file.stat().st_size == metadata.st_size,
                "destroy discarded current generated secret source/binding")
        self.swarm_secret(project, name, secret_before)
        self.discard_scratch(project)
        self.report.append("disposable single/two-node Swarm: external generated source, full build/push/digest distribution and updates, native rollback, strict status and replica faults, failed rollout/data retention, conservative multi-node destroy and owned teardown")
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
        if self.host_daemon_id:
            require(self.docker("info", "--format", "{{.ID}}", env=self.host_env).stdout.strip() ==
                    self.host_daemon_id, "outer Docker target changed; refusing cleanup")
        fixtures = []
        for path, saved_name, environment in reversed(self.projects):
            project = Path(path).resolve()
            try:
                require(project.is_relative_to(self.root.resolve()), "cleanup escaped run-owned fixture")
                name = saved_name or self.cli(project, "config", "get", "project", env=environment)["value"]
                backend = self.cli(project, "config", "get", "backend", env=environment)["value"]
                require(self.cli(project, "config", "get", "project", env=environment)["value"] == name,
                        "tracked fixture project name changed")
                if backend == "swarm":
                    require(environment == self.node_envs.get("manager"),
                            "refusing Swarm cleanup outside pinned disposable manager")
                fixtures.append((backend, project, str(project), name, environment))
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append(f"{project}: {error}")
        if not errors:
            try:
                # Prove both outer owners and the complete cluster before any down.
                # Stop all tracked deployments before retiring their shared worker.
                self.verified_swarm_nodes()
                for backend, project, _, _, environment in fixtures:
                    if backend == "swarm":
                        self.cli(project, "down", env=environment, timeout=180)
                self.retire_swarm_worker()
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append("disposable Swarm retirement: " + str(error))
        if errors:
            raise SmokeFailure("fixture cleanup failed; recovery retained at " + str(self.root) +
                               ":\n" + "\n".join(errors))
        for backend, project, owner, name, environment in fixtures:
            try:
                if backend == "swarm":
                    self.verified_dind_node("manager")
                else:
                    require(self.docker("info", "--format", "{{.ID}}", env=environment).stdout.strip() ==
                            self.host_daemon_id, "Compose cleanup target changed")
                self.cli(project, "destroy", "--yes", env=environment, timeout=180)
                if backend == "swarm":
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
                for kind, listing in (("containers", ("ps", "-aq")),
                                      ("networks", ("network", "ls", "-q")),
                                      ("volumes", ("volume", "ls", "-q"))):
                    for label in ("io.dockstride.owner=" + owner, "io.dockstride.project=" + name):
                        remaining = self.docker(*listing, "--filter", "label=" + label,
                                                env=environment).stdout.split()
                        require(not remaining, "owned " + kind + " remain: " + repr(remaining))
                self.assert_disposable_state(project)
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, SmokeFailure) as error:
                errors.append(f"{project}: {error}")
        if errors:
            # Preserve exact connections and credentials if teardown is uncertain.
            raise SmokeFailure("fixture cleanup failed; recovery retained at " + str(self.root) +
                               ":\n" + "\n".join(errors))
        for name in reversed(self.containers):
            try:
                item = self.verified_outer_container(name)
                if item:
                    result = self.docker("rm", "-f", "-v", item["Id"], env=self.host_env, ok=False)
                    if result.returncode:
                        errors.append(result.stderr)
                    else:
                        require(self.verified_outer_container(name) is None, "outer container survived removal")
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
            require(item.get("Name") == self.network and item.get("Id") == self.network_id and
                    item.get("Labels", {}).get("dockstride.smoke") == self.prefix,
                    "outer fixture network ownership changed; retaining recovery")
            result = self.docker("network", "rm", item["Id"], env=self.host_env, ok=False)
            if result.returncode:
                raise SmokeFailure("outer network cleanup failed; recovery retained at " + str(self.root) +
                                   ":\n" + result.stderr)
        for name in reversed(self.contexts):
            context = self.json_docker("context", "inspect", name, env=self.host_env)[0]
            records = [node for node in self.dind_nodes.values() if node["context"] == name]
            require(len(records) == 1 and context.get("Name") == name and
                    context.get("Endpoints", {}).get("docker", {}).get("Host") == records[0]["endpoint"],
                    "context target changed; refusing context cleanup")
            result = self.docker("context", "rm", "-f", name, env=self.host_env, ok=False)
            if result.returncode:
                errors.append(result.stderr)
        if errors:
            raise SmokeFailure("outer fixture cleanup failed:\n" + "\n".join(errors))

    def compose_private_file(self, path, content, mode=0o600):
        path.parent.mkdir(parents=True, exist_ok=True)
        flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW
        descriptor = os.open(path, flags, mode)
        with os.fdopen(descriptor, "wb") as output:
            os.fchmod(output.fileno(), mode)
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
                              "  wrongIdentity | Bool | default = false,")
        model = model.replace('json = { application = "dockstride-sample", project = env.project, uid = env.containerUid },',
                              'json = { application = "dockstride-sample", project = if env.wrongIdentity then "wrong-fixture-identity" else env.project, uid = env.containerUid },')
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



    def compose_postgres_smoke(self):
        project = self.checkout(self.prefix + "-postgres")
        name = self.prefix + "-postgres"
        self.projects.append((project, name, self.env.copy()))
        password = self.root / "postgres.input"
        self.compose_private_file(password, uuid.uuid4().hex + "\n", mode=0o640)
        model = r'''let lib = import "libs/dockstride.ncl" in
let cfg = {
  project | String, backend | lib.Backend | default = "compose",
  hostSecretUid | Number, hostSecretGid | Number,
  postgresDb | String | default = "fixture",
  secrets = { password | lib.SecretSource },
} in
let env | cfg = import "env.yaml" in
let dc = lib.forEnvironment env in
dc.ComposeFile {
  dockstride | not_exported = {
    Config = cfg,
    setup = {
      secrets.password = lib.ReferenceSecret,
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
        inputs = {"project": name, "hostSecretUid": os.getuid(), "hostSecretGid": os.getgid()}
        argv = ["setup", "--secret-file", "password=" + str(password)]
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
        self.compose_private_file(wrong, uuid.uuid4().hex + "\n", mode=0o640)
        self.cli(project, "secrets", "replace", "password", "--file", str(wrong))
        error = self.compose_bounded_cli(project, 15, "up", fail=True)
        finding = self.compose_postgres_finding(project, error["details"]["diagnostics"], "authentication-failed")
        require(finding.get("suggestedAction") and error["category"] == "operation" and error["exitCode"] == 1,
                "credential mismatch did not retain primary failure and inert recovery guidance")
        require(password.is_file(), "diagnosis removed original provider file")
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
        expected = {str(Path(path).resolve()) for path, saved_name, _ in self.projects if saved_name == name}
        require(len(expected) == 1 and labels.get("io.dockstride.project") == name and
                labels.get("io.dockstride.owner") in expected,
                "refusing to manipulate a foreign Swarm service")
        return item


    def swarm_secret_ids(self, name, owner):
        return set(self.docker("secret", "ls", "-q", "--filter", "label=io.dockstride.owner=" + owner,
                               "--filter", "label=io.dockstride.project=" + name).stdout.split())


    def swarm_secret(self, project, name, reference, owner=None):
        require(reference.get("external") is True and isinstance(reference.get("name"), str),
                "Swarm credential is not an immutable external reference")
        item = self.json_docker("secret", "inspect", reference["name"])[0]
        labels = item["Spec"].get("Labels", {})
        current_owner = str(project.resolve())
        require(labels.get("io.dockstride.owner") == current_owner and
                (owner is None or current_owner == owner) and
                labels.get("io.dockstride.project") == name and
                labels.get("io.dockstride.secret") == "authKey" and
                item.get("ID"),
                "Swarm secret lacks exact path/project/logical ownership labels")
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
        self.cli(project, "deploy", timeout=900)
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
                rows[0].get("source", {}).get("canonicalPath") == str(source.resolve()) and
                rows[0].get("source", {}).get("origin") is not None and
                set(rows[0]["source"]) == {"canonicalPath", "origin"} and
                set(rows[0]) == {"name", "status", "source", "reference", "binding", "consumers",
                                "applied", "consumerRestartNeeded"},
                "secret sync lost current source provenance or current-publication result shape")
        return rows[0]




    def swarm_import_sync_smoke(self):
        project = self.checkout("swarm-file")
        source = self.root / "swarm-provider-source"
        source_values = [uuid.uuid4().hex + uuid.uuid4().hex for _ in range(3)]
        self.swarm_private_write(source, source_values[0])
        shared = self.root / "swarm-shared.yaml"
        self.compose_private_file(shared, "secrets:\n  authKey:\n    file: " + json.dumps(str(source.resolve())) + "\n")
        shared_bytes = shared.read_bytes()
        model_path = project / "compose.ncl"
        model = model_path.read_text()
        policy = 'lib.GenerateSecret { bytes = 32, encoding = "hex" }'
        require(model.count(policy) == 1, "sample generated-secret policy changed")
        model = model.replace(policy, "lib.ReferenceSecret")
        self.swarm_private_write(model_path, model)
        self.cli(project, "config", "sources", "add", str(shared))
        name = self.setup(project, "swarm-file", "swarm", self.app_port)
        original = self.swarm_ref(project)
        original_object = self.swarm_secret(project, name, original)
        self.current_secret_listing(project, "swarm", {"file": str(source.resolve())}, original["name"])
        owner = str(project.resolve())
        require(str(source) not in (project / "env.yaml").read_text() and shared.read_bytes() == shared_bytes,
                "initial provisioning copied shared provider reference into local ordinary settings")
        before_plan = (project / "env.yaml").read_bytes()
        initial_ids = self.swarm_secret_ids(name, owner)
        plan = self.cli(project, "secrets", "sync", "authKey", "--plan")
        require(plan.get("sideEffects") is False and plan.get("committed") == [] and
                plan.get("applied") == [] and plan["secrets"][0]["status"] == "planned" and
                plan["secrets"][0]["source"]["canonicalPath"] == str(source.resolve()) and
                (project / "env.yaml").read_bytes() == before_plan and
                self.swarm_secret_ids(name, owner) == initial_ids,
                "sync plan consumed/publicized material or changed current metadata")
        republished = self.cli(project, "secrets", "sync", "authKey", "--yes")
        row = self.swarm_sync_report(republished, source, "published", ["authKey"], [])
        first = self.swarm_ref(project)
        first_object = self.swarm_secret(project, name, first)
        require(first != original and first_object["ID"] != original_object["ID"] and
                row["reference"] == {"file": str(source.resolve())} and row["binding"] == first["name"],
                "explicit unchanged-byte sync did not create a fresh binding")
        self.cli(project, "deploy", timeout=900)
        identity = self.identity(self.app_port, name)
        self.swarm_private_write(source, source_values[1])
        current_ids = self.swarm_secret_ids(name, owner)
        self.cli(project, "setup")
        self.cli(project, "deploy", timeout=900)
        require(self.swarm_ref(project) == first and self.swarm_secret_ids(name, owner) == current_ids and
                self.identity(self.app_port, name)["secretFingerprint"] == identity["secretFingerprint"],
                "ordinary setup/deploy implicitly republished changed provider bytes")
        rejected = self.cli(project, "secrets", "sync", "authKey", "--yes", "--apply", fail=True)
        require("rotation" in rejected.get("message", "").lower() and
                self.swarm_ref(project) == first and self.swarm_secret_ids(name, owner) == current_ids,
                "undeclared rotation was not rejected before backend publication")
        changed = self.cli(project, "secrets", "sync", "authKey", "--yes")
        row = self.swarm_sync_report(changed, source, "published", ["authKey"], [])
        second = self.swarm_ref(project)
        second_object = self.swarm_secret(project, name, second)
        require(second != first and second_object["ID"] != first_object["ID"] and
                row["reference"] == {"file": str(source.resolve())} and row["binding"] == second["name"] and
                row["consumerRestartNeeded"] is True and shared.read_bytes() == shared_bytes and
                str(source) not in (project / "env.yaml").read_text(),
                "sync failed to switch only the local binding of a shared original reference")
        for service in ("api", "worker"):
            grants = self.swarm_service(name, service)["Spec"]["TaskTemplate"]["ContainerSpec"]["Secrets"]
            require(any(grant.get("SecretID") == first_object["ID"] for grant in grants),
                    "storage-only sync unexpectedly changed live service grants")
        blocked = self.docker("secret", "rm", first_object["ID"], ok=False)
        require(blocked.returncode != 0 and "in use" in (blocked.stderr + blocked.stdout).lower(),
                "live Swarm grant did not protect the old immutable object")
        marker = project / "rotation-command-ran"
        rotation = ('{ name = "deliberate-rotation-failure", kind = "command", '
                    'argv = ["python3", "-c", ' + json.dumps(
                        "from pathlib import Path; import os; p=Path(" + repr(str(marker)) +
                        "); fd=os.open(p, os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600); "
                        "os.close(fd); raise SystemExit(23)") +
                    '], workflows = ["fixture-rotate-auth"], stage = "before", services = ["api", "worker"] }')
        rotating_model = model.replace("] else [],\n    oneshots =",
                                       "] else [" + rotation + "],\n    oneshots =", 1)
        require(rotating_model.count("      secretAccess.authKey =") == 1 and rotating_model != model,
                "sample metadata changed before fixture rotation declaration")
        rotating_model = rotating_model.replace("      secretAccess.authKey =",
            '      rotations.authKey = { workflow = "fixture-rotate-auth", services = ["api", "worker"] },\n'
            "      secretAccess.authKey =")
        self.swarm_private_write(model_path, rotating_model)
        self.swarm_private_write(source, source_values[2])
        failed = self.cli(project, "secrets", "sync", "authKey", "--yes", "--apply", fail=True)
        report = failed.get("details", {}).get("secretSync")
        require(isinstance(report, dict), "failed apply omitted precise details.secretSync")
        row = self.swarm_sync_report(report, source, "published", ["authKey"], [])
        third = self.swarm_ref(project)
        third_object = self.swarm_secret(project, name, third)
        require(third not in (first, second) and third_object["ID"] not in (first_object["ID"], second_object["ID"]) and
                row["binding"] == third["name"] and row["applied"] is False and
                row["consumerRestartNeeded"] is True and marker.is_file() and
                marker.stat().st_uid == os.getuid() and marker.stat().st_mode & 0o777 == 0o600,
                "failed explicit command rotation did not retain committed binding and execution evidence")
        for reference in (original, first, second, third):
            self.swarm_secret(project, name, reference)
        require(self.identity(self.app_port, name)["secretFingerprint"] == identity["secretFingerprint"],
                "failed inert rotation changed live consumer credentials")
        serialized = json.dumps((plan, republished, rejected, changed, failed))
        require(all(value not in serialized for value in source_values),
                "Swarm sync/apply output leaked credential bytes")
        self.swarm_private_write(model_path, model)
        self.cli(project, "deploy", timeout=900)
        require(self.identity(self.app_port, name)["secretFingerprint"] != identity["secretFingerprint"],
                "explicit full deploy did not apply committed current source publication")
        for service in ("api", "worker"):
            grants = self.swarm_service(name, service)["Spec"]["TaskTemplate"]["ContainerSpec"]["Secrets"]
            require(any(grant.get("SecretID") == third_object["ID"] for grant in grants),
                    "full deploy did not consume the current immutable binding")
        self.cli(project, "down")
        self.discard_scratch(project)
        require(self.swarm_ref(project) == third and shared.read_bytes() == shared_bytes and marker.is_file(),
                "down or scratch removal changed committed reference, source, or failed-action evidence")
        self.cli(project, "destroy", "--yes")
        for reference in (original, first, second, third):
            self.swarm_secret(project, name, reference)
        self.report.append("Swarm shared original-file provenance, no-effect plan, fresh every-sync immutable publication, ordinary setup reuse, preflight rotation refusal, committed failed explicit rotation, live old grants, explicit full apply, retained old objects and scratch-independent teardown")





def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dks", default="target/debug/dks", help="already-built CLI; this harness never builds Rust")
    parser.add_argument("--compose", action="store_true", help="exercise Compose with run-owned checkout paths/project names")
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
