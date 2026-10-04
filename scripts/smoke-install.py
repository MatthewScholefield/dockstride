#!/usr/bin/env python3
"""Exercise the installer against real HTTP assets and the native release binary."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def run(argv, env):
    return subprocess.run(argv, env=env, text=True, capture_output=True, timeout=120)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", required=True, type=Path)
    args = parser.parse_args()
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(platform.machine())
    require(platform.system() == "Linux" and arch is not None, "native Linux x86_64/ARM64 required")
    target = arch + "-unknown-linux-musl"
    match = re.fullmatch(r"dockstride-(.+)-" + re.escape(target) + r"\.tar\.gz", args.archive.name)
    require(match is not None, "archive does not match this native platform")
    version = match[1]
    archive = args.archive.read_bytes()
    installer = Path(__file__).resolve().with_name("install.sh")
    real_curl = shutil.which("curl")
    require(real_curl is not None, "curl is required")
    requests = []
    mode = "ok"
    wrong_version = "0.0.0" if version != "0.0.0" else "0.0.1"
    wrong_name = f"dockstride-{wrong_version}-{target}.tar.gz"
    wrong_archive = io.BytesIO()
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as source:
        binary = source.extractfile(f"dockstride-{version}-{target}/dks").read()
    with tarfile.open(fileobj=wrong_archive, mode="w:gz") as destination:
        member = tarfile.TarInfo(f"dockstride-{wrong_version}-{target}/dks")
        member.size, member.mode, member.mtime = len(binary), 0o755, 0
        destination.addfile(member, io.BytesIO(binary))
    wrong_bytes = wrong_archive.getvalue()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            requests.append(self.path)
            if self.path == "/releases/latest":
                self.send_response(302)
                self.send_header("Location", f"http://127.0.0.1:{self.server.server_port}/releases/tag/v{version}")
                self.end_headers()
                return
            if self.path == f"/releases/tag/v{version}":
                data = b"Local release fixture"
            elif self.path.startswith("/releases/download/"):
                name = self.path.rsplit("/", 1)[-1]
                if mode == "missing":
                    self.send_error(404)
                    return
                selected = wrong_bytes if mode == "version" else archive
                expected = wrong_name if mode == "version" else args.archive.name
                if name == expected:
                    data = selected
                elif name == expected + ".sha256":
                    checksum = "0" * 64 if mode == "corrupt" else hashlib.sha256(selected).hexdigest()
                    filename = "../foreign-file" if mode == "filename" else expected
                    data = f"{checksum}  {filename}\n".encode()
                else:
                    self.send_error(404)
                    return
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        with tempfile.TemporaryDirectory(prefix="dockstride-installer-smoke-") as temporary:
            root = Path(temporary)
            shim = root / "commands"
            shim.mkdir()
            # Only the URL origin changes. Downloads, redirects, checksum validation,
            # archive extraction, process execution, and installation are all real.
            (shim / "curl").write_text(
                f"#!{sys.executable}\nimport os,sys\n"
                "prefix='https://github.com/MatthewScholefield/dockstride'\n"
                f"base='http://127.0.0.1:{server.server_port}'\n"
                "args=[base+a[len(prefix):] if a.startswith(prefix+'/') else a for a in sys.argv[1:]]\n"
                f"os.execv({real_curl!r}, [{real_curl!r}, *args])\n"
            )
            (shim / "curl").chmod(0o755)
            env = os.environ.copy()
            env.pop("DOCKSTRIDE_VERSION", None)
            env.pop("DOCKSTRIDE_INSTALL_DIR", None)
            env.update(HOME=str(root / "home with spaces"), TMPDIR=str(root), PATH=str(shim) + os.pathsep + env["PATH"])
            result = run(["sh", str(installer)], env)
            require(result.returncode == 0, result.stdout + result.stderr)
            installed = Path(env["HOME"]) / ".local/bin/dks"
            require(run([str(installed), "--version"], env).stdout.strip() == f"dks {version}", "installed version differs")
            require(installed.read_bytes() == binary, "installer did not preserve the verified release binary")
            app = root / "application"
            app.mkdir()
            result = run([str(installed), "-C", str(app), "--json", "init"], env)
            require(result.returncode == 0, result.stderr)
            result = run([str(installed), "-C", str(app), "--json", "config", "schema"], env)
            require(result.returncode == 0, result.stderr)
            fields = json.loads(result.stdout.splitlines()[-1])["result"]["fields"]
            require({item["path"] for item in fields} == {"project", "backend", "apiPort"}, "installed starter schema differs")
            require(any(item["path"] == "project" and item["required"] for item in fields), "installed evaluator lost required project")
            require(any(item["path"] == "backend" and item["default"] == "compose" for item in fields), "installed evaluator lost backend default")
            require(any(item["path"] == "apiPort" and item["default"] == 8080 for item in fields), "installed evaluator lost port default")
            print("PASS latest release HTTP download, exact binary install, and embedded configuration schema")

            custom = root / "custom bin"
            env.update(DOCKSTRIDE_VERSION="v" + version, DOCKSTRIDE_INSTALL_DIR=str(custom))
            requests.clear()
            result = run(["sh", str(installer)], env)
            require(result.returncode == 0, result.stdout + result.stderr)
            require((custom / "dks").read_bytes() == binary, "pinned custom install differs")
            require("/releases/latest" not in requests, "pinned version resolved latest")
            print("PASS pinned version and custom directory with spaces")

            for failure in ("corrupt", "filename", "missing", "version"):
                mode = failure
                env["DOCKSTRIDE_VERSION"] = "v" + (wrong_version if failure == "version" else version)
                result = run(["sh", str(installer)], env)
                require(result.returncode != 0, f"{failure} unexpectedly installed")
                require((custom / "dks").read_bytes() == binary, f"{failure} changed installed binary")
                require(not list(custom.glob(".dockstride-install.*")), "failed install left staging")
                require(not list(root.glob("dockstride-install.*")), "failed install left downloads")
                print(f"PASS {failure} failure preserves existing binary and cleans temporary state")

            (shim / "uname").write_text("#!/bin/sh\nprintf '%s\\n' Darwin\n")
            (shim / "uname").chmod(0o755)
            requests.clear()
            unused = root / "unsupported destination"
            env["DOCKSTRIDE_INSTALL_DIR"] = str(unused)
            result = run(["sh", str(installer)], env)
            require(result.returncode != 0 and "unsupported operating system" in result.stderr, result.stdout + result.stderr)
            require(not requests and not unused.exists(), "unsupported platform had effects")
            print("PASS unsupported platform rejects before network or destination changes")
    finally:
        server.shutdown()
        server.server_close()
        worker.join()


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"INSTALLER SMOKE FAILED: {error}", file=sys.stderr)
        sys.exit(1)
