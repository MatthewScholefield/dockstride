#!/usr/bin/env python3
"""Return configuration proposals; Dockstride owns all publication and allocation."""
import json
from pathlib import Path
import sys

context = json.load(sys.stdin)
if context.get("schemaVersion") != 1:
    raise SystemExit("Unsupported Dockstride context schemaVersion")
values = {}
if "project" in context.get("missingFields", []):
    values["project"] = context["projectProposal"]
response = {"schemaVersion": 1, "values": values}
if not context.get("sourcesDeclared", False):
    response["sources"] = [{"path": str(Path(context["checkout"]) / "env.shared.yaml"), "createIfMissing": True}]
json.dump(response, sys.stdout)
sys.stdout.write("\n")
