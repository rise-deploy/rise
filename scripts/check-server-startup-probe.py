#!/usr/bin/env python3
"""Verify the rendered server startup budget and explicit chart overrides."""

from pathlib import Path
import re
import subprocess


CHART = Path(__file__).resolve().parents[1] / "helm" / "rise"


def deployment(*overrides):
    return subprocess.run(
        ["helm", "template", "startup-check", str(CHART),
         "--show-only", "templates/deployment.yaml", *overrides],
        check=True, capture_output=True, text=True, timeout=30,
    ).stdout


def probe(document, name):
    match = re.search(rf"(?m)^        {name}:\n((?:          .*\n)+)", document)
    return match.group(1) if match else None


default = deployment()
startup = probe(default, "startupProbe")
assert startup is not None, "Server startup probe is required by default"
for field in ["path: /health", "port: http", "periodSeconds: 10",
              "timeoutSeconds: 5", "failureThreshold: 180"]:
    assert field in startup, f"Missing startup probe setting: {field}"
assert "path: /health" in probe(default, "readinessProbe")
assert "path: /health" in probe(default, "livenessProbe")

custom = probe(deployment("--set", "server.startupProbe.failureThreshold=360"), "startupProbe")
assert "failureThreshold: 360" in custom, "Startup budget must be configurable"
assert probe(deployment("--set", "server.startupProbe=null"), "startupProbe") is None
print("Server startup probe defaults, override and explicit disable: passed")
