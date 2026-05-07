#!/usr/bin/env python3
"""
Test JFrog artifact scope patterns against Docker push and BuildKit push.

Requires:
  - JFrog Artifactory running on localhost:3082 with rise-docker-local repo
  - JFROG_ADMIN_TOKEN env var set (applied-permissions/admin scoped token)
  - docker CLI available
  - docker buildx with "rise-buildkit" builder (remote BuildKit daemon)

Usage:
  export JFROG_ADMIN_TOKEN=...
  python3 jfrog_scope_test.py
"""

import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass

JFROG_URL = "http://localhost:3082"
REGISTRY = "localhost:3082"
REPO_KEY = "rise-docker-local"
IMAGE_NAME = "hello-world"
BUILDX_BUILDER = "rise-buildkit"

ADMIN_TOKEN = os.environ.get("JFROG_ADMIN_TOKEN", "")


@dataclass
class Result:
    label: str
    method: str
    scope: str
    success: bool
    error: str


def mint_token(scope: str) -> tuple[str, str]:
    """Mint a scoped JFrog access token. Returns (username, token)."""
    resp = subprocess.run(
        [
            "curl", "-sf",
            "-H", f"Authorization: Bearer {ADMIN_TOKEN}",
            "-H", "Content-Type: application/json",
            "-d", json.dumps({"scope": scope, "expires_in": 600}),
            f"{JFROG_URL}/access/api/v1/tokens",
        ],
        capture_output=True, text=True,
    )
    if resp.returncode != 0 or not resp.stdout.strip():
        raise RuntimeError(f"Token request failed (rc={resp.returncode}): {resp.stderr.strip()}")
    try:
        data = json.loads(resp.stdout)
    except json.JSONDecodeError:
        raise RuntimeError(f"Token response not JSON: {resp.stdout[:200]}")
    username = data.get("username", "admin")
    return username, data["access_token"]


def docker_login(username: str, token: str) -> bool:
    resp = subprocess.run(
        ["docker", "login", REGISTRY, "-u", username, "--password-stdin"],
        input=token, capture_output=True, text=True,
    )
    return resp.returncode == 0


def build_unique_image(tag: str) -> bool:
    """Build a unique image locally so layers aren't cached in the registry."""
    unique = str(time.time_ns())
    dockerfile = f"FROM alpine:3.20\nRUN echo {unique} > /unique.txt\n"
    resp = subprocess.run(
        ["docker", "build", "-t", f"{REGISTRY}/{REPO_KEY}/{IMAGE_NAME}:{tag}", "-"],
        input=dockerfile, capture_output=True, text=True,
    )
    return resp.returncode == 0


def test_docker_push(tag: str) -> tuple[bool, str]:
    """Push using docker push (separate from build)."""
    resp = subprocess.run(
        ["docker", "push", f"{REGISTRY}/{REPO_KEY}/{IMAGE_NAME}:{tag}"],
        capture_output=True, text=True,
    )
    if resp.returncode == 0:
        return True, ""
    output = resp.stderr + resp.stdout
    for line in output.splitlines():
        low = line.lower()
        if "unauthorized" in low or "denied" in low or "error" in low:
            return False, line.strip()
    return False, output.strip()[-200:]


def test_buildx_push(tag: str) -> tuple[bool, str]:
    """Push using docker buildx build --push with the remote BuildKit daemon."""
    unique = str(time.time_ns())
    dockerfile = f"FROM alpine:3.20\nRUN echo {unique} > /unique.txt\n"

    df_path = f"/tmp/jfrog-scope-test-{tag}.Dockerfile"
    ctx_path = "/tmp/jfrog-scope-test-ctx"
    os.makedirs(ctx_path, exist_ok=True)
    with open(df_path, "w") as f:
        f.write(dockerfile)

    resp = subprocess.run(
        [
            "docker", "buildx", "build",
            "--builder", BUILDX_BUILDER,
            "--push",
            "-t", f"{REGISTRY}/{REPO_KEY}/{IMAGE_NAME}:{tag}",
            "-f", df_path,
            ctx_path,
        ],
        capture_output=True, text=True,
    )
    try:
        os.unlink(df_path)
    except OSError:
        pass

    if resp.returncode == 0:
        return True, ""
    output = resp.stderr + resp.stdout
    for line in output.splitlines():
        low = line.lower()
        if "unauthorized" in low or "denied" in low:
            return False, line.strip()
        if "ERROR:" in line:
            return False, line.strip()
    return False, output.strip()[-200:]


def make_scope(label: str, tag: str) -> str:
    """Build the scope string for a given label and tag."""
    base = f"{REPO_KEY}/{IMAGE_NAME}"
    scope_map = {
        "{project}/**":
            f"artifact:{base}/**:r,w",
        "{project}/*":
            f"artifact:{base}/*:r,w",
        "{tag}/** + _uploads/**":
            f"artifact:{base}/{tag}/**:r,w artifact:{base}/_uploads/**:r,w",
        "{tag}/*.json + _uploads/**":
            f"artifact:{base}/{tag}/*.json:r,w artifact:{base}/_uploads/**:r,w",
        "{tag}/* + _uploads/**":
            f"artifact:{base}/{tag}/*:r,w artifact:{base}/_uploads/**:r,w",
        "{tag}/** + _uploads/** + sha256__*/**":
            f"artifact:{base}/{tag}/**:r,w artifact:{base}/_uploads/**:r,w artifact:{base}/sha256__*/**:r,w",
        "{tag}/** + _uploads/** + sha256*/**":
            f"artifact:{base}/{tag}/**:r,w artifact:{base}/_uploads/**:r,w artifact:{base}/sha256*/**:r,w",
        "{tag}/** + _uploads/** + sha256*/*":
            f"artifact:{base}/{tag}/**:r,w artifact:{base}/_uploads/**:r,w artifact:{base}/sha256*/*:r,w",
        "{project}/**  (w only)":
            f"artifact:{base}/**:w",
        "{tag}/** + _uploads/** + sha256*/*  (w only)":
            f"artifact:{base}/{tag}/**:w artifact:{base}/_uploads/**:w artifact:{base}/sha256*/*:w",
        "{tag}/** + _uploads/** + sha256*/**  (w only)":
            f"artifact:{base}/{tag}/**:w artifact:{base}/_uploads/**:w artifact:{base}/sha256*/**:w",
    }
    scope = scope_map.get(label)
    if scope is None:
        raise ValueError(f"Unknown scope label: {label}")
    return scope


SCOPE_LABELS = [
    "{project}/**",
    "{project}/*",
    "{tag}/** + _uploads/**",
    "{tag}/*.json + _uploads/**",
    "{tag}/* + _uploads/**",
    "{tag}/** + _uploads/** + sha256__*/**",
    "{tag}/** + _uploads/** + sha256*/**",
    "{tag}/** + _uploads/** + sha256*/*",
    "{project}/**  (w only)",
    "{tag}/** + _uploads/** + sha256*/*  (w only)",
    "{tag}/** + _uploads/** + sha256*/**  (w only)",
]


def run_test(label: str, method: str, tag: str) -> Result:
    scope = make_scope(label, tag)

    try:
        username, token = mint_token(scope)
    except RuntimeError as e:
        return Result(label, method, scope, False, f"Token mint failed: {e}")

    if not docker_login(username, token):
        return Result(label, method, scope, False, "Docker login failed")

    if method == "docker push":
        if not build_unique_image(tag):
            return Result(label, method, scope, False, "Local build failed")
        ok, err = test_docker_push(tag)
    elif method == "buildx --push":
        ok, err = test_buildx_push(tag)
    else:
        return Result(label, method, scope, False, f"Unknown method: {method}")

    return Result(label, method, scope, ok, err)


def main():
    if not ADMIN_TOKEN:
        print("ERROR: Set JFROG_ADMIN_TOKEN environment variable", file=sys.stderr)
        sys.exit(1)

    # Check prerequisites
    for cmd in [["docker", "version"], ["docker", "buildx", "version"]]:
        if subprocess.run(cmd, capture_output=True).returncode != 0:
            print(f"ERROR: {cmd[0]} not available", file=sys.stderr)
            sys.exit(1)

    # Check buildx builder exists
    resp = subprocess.run(
        ["docker", "buildx", "inspect", BUILDX_BUILDER],
        capture_output=True, text=True,
    )
    has_buildkit = resp.returncode == 0
    if not has_buildkit:
        print(f"WARNING: BuildKit builder '{BUILDX_BUILDER}' not found, skipping buildx tests")

    methods = ["docker push"]
    if has_buildkit:
        methods.append("buildx --push")

    ts = int(time.time())
    results: list[Result] = []
    test_num = 0

    for method in methods:
        for label in SCOPE_LABELS:
            tag = f"st-{ts}-{test_num}"
            test_num += 1
            print(f"  Testing: {label:45s} via {method:16s} ... ", end="", flush=True)
            result = run_test(label, method, tag)
            results.append(result)
            status = "PASS" if result.success else f"FAIL: {result.error[:80]}"
            print(status)

    # Summary table
    print()
    print("=" * 100)
    print(f"{'Scope pattern':<50} {'docker push':>14} {'buildx --push':>14}")
    print("-" * 100)

    for label in SCOPE_LABELS:
        row: dict[str, bool | None] = {}
        for r in results:
            if r.label == label:
                row[r.method] = r.success

        cells = []
        for m in methods:
            v = row.get(m)
            if v is None:
                cells.append("skip")
            elif v:
                cells.append("PASS")
            else:
                cells.append("FAIL")

        col1 = f"{cells[0]:>14}"
        col2 = f"{cells[1]:>14}" if len(cells) > 1 else f"{'skip':>14}"
        print(f"{label:<50} {col1} {col2}")

    print("=" * 100)

    failures = [r for r in results if not r.success]
    if failures:
        print(f"\n{len(failures)} test(s) failed.")
    else:
        print("\nAll tests passed.")


if __name__ == "__main__":
    main()
