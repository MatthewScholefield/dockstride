#!/usr/bin/env python3
import os
from pathlib import Path
import sys

count = Path("/data/migration-runs")
attempt = (int(count.read_text()) if count.exists() else 0) + 1
count.write_text(f"{attempt}\n")
print(f"sample migration attempt {attempt}", flush=True)
if os.environ.get("FAIL_MIGRATION") == "true":
    print("sample migration failed: deliberately refused schema upgrade", file=sys.stderr, flush=True)
    sys.exit(17)
Path("/data/migrated").write_text("schema version 1\n")
print("sample migration completed: schema version 1", flush=True)
