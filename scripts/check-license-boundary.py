#!/usr/bin/env python3
"""Fail if a permissively licensed workspace crate depends on a BUSL-1.1 one.

The BSL crates (rise-backend-kubernetes, rise-backend-ecs) may only be linked
by `rise-deploy`, behind its `backend` feature, which is what builds the
server. Every other workspace crate is MIT OR Apache-2.0 and must not reach a
BSL crate through any dependency path, with all features enabled. See
LICENSING.md.

cargo-deny cannot enforce this: first-party crates are `publish = false`, and
deny.toml ignores private packages.
"""

import json
import subprocess
import sys

BSL = "BUSL-1.1"
# The root crate links the BSL backends by design: its `backend` feature
# builds the server, and LICENSING.md documents the server as BSL-bearing.
ALLOWED_TO_LINK_BSL = {"rise-deploy"}


def main() -> int:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(result.stderr, end="", file=sys.stderr)
        print("error: `cargo metadata` failed", file=sys.stderr)
        return 1
    metadata = json.loads(result.stdout)
    packages = {p["id"]: p for p in metadata["packages"]}
    members = set(metadata["workspace_members"])
    bsl = {pid for pid in members if packages[pid].get("license") == BSL}
    if not bsl:
        print("No BUSL-1.1 workspace crates; nothing to check.")
        return 0

    # Normal and build dependencies only: a dev-dependency never ships.
    edges = {}
    for node in metadata["resolve"]["nodes"]:
        edges[node["id"]] = [
            dep["pkg"]
            for dep in node["deps"]
            if any(k["kind"] in (None, "build") for k in dep["dep_kinds"])
        ]

    failed = False
    for pid in sorted(members, key=lambda i: packages[i]["name"]):
        name = packages[pid]["name"]
        if pid in bsl or name in ALLOWED_TO_LINK_BSL:
            continue
        path = find_path(pid, bsl, edges)
        if path:
            chain = " -> ".join(packages[p]["name"] for p in path)
            print(f"error: permissive crate {name} depends on BUSL-1.1 code: {chain}")
            failed = True

    if failed:
        print("See LICENSING.md: nothing permissive may depend on a BSL crate.")
        return 1
    names = ", ".join(sorted(packages[p]["name"] for p in bsl))
    print(f"License boundary holds: no permissive crate depends on {names}.")
    return 0


def find_path(start, targets, edges):
    """Breadth-first search for a dependency path from start into targets."""
    parents = {start: None}
    queue = [start]
    while queue:
        node = queue.pop(0)
        for dep in edges.get(node, []):
            if dep in parents:
                continue
            parents[dep] = node
            if dep in targets:
                path = [dep]
                while parents[path[-1]] is not None:
                    path.append(parents[path[-1]])
                return list(reversed(path))
            queue.append(dep)
    return None


if __name__ == "__main__":
    sys.exit(main())
