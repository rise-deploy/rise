#!/usr/bin/env python3
"""Verify supported ECS state layouts using isolated, mocked Terraform upgrades.

Requires the baseline Git object and Terraform on PATH. The production module's
initialized provider lock and binaries are reused. No AWS credentials or real
infrastructure are used.
"""

import argparse
import json
import re
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path

BASELINE = "348dd9e2ce2ff0eea15ab3cf44728bc073d46850"
PATHS = ["modules/rise-ecs", "tests/e2e/run", "dev/dex/config.yaml"]
MOCKS = r"""
mock_provider "aws" {
  mock_data "aws_caller_identity" { defaults = { account_id = "123456789012" } }
  mock_data "aws_region" { defaults = { id = "eu-central-1", region = "eu-central-1" } }
  mock_data "aws_partition" { defaults = { partition = "aws" } }
  mock_data "aws_availability_zones" { defaults = { names = ["eu-central-1a", "eu-central-1b"] } }
  mock_data "aws_subnet" { defaults = { vpc_id = "vpc-0123456789abcdef0" } }
  mock_data "aws_ecs_cluster" { defaults = { arn = "arn:aws:ecs:eu-central-1:123456789012:cluster/existing", status = "ACTIVE" } }
  mock_data "aws_iam_policy_document" { defaults = { json = "{}" } }
  mock_resource "aws_eip" { defaults = { public_ip = "192.0.2.10" } }
  mock_resource "aws_ecs_cluster" { defaults = { arn = "arn:aws:ecs:eu-central-1:123456789012:cluster/rise" } }
  mock_resource "aws_iam_role" { defaults = { arn = "arn:aws:iam::123456789012:role/rise-traefik" } }
  mock_resource "aws_secretsmanager_secret" { defaults = { arn = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:rise/example-abc123" } }
  mock_resource "aws_efs_file_system" { defaults = { arn = "arn:aws:elasticfilesystem:eu-central-1:123456789012:file-system/fs-abc", id = "fs-abc" } }
  mock_resource "aws_efs_access_point" { defaults = { id = "fsap-abc" } }
  mock_resource "aws_service_discovery_service" { defaults = { arn = "arn:aws:servicediscovery:eu-central-1:123456789012:service/srv-abc" } }
  mock_resource "aws_ecs_task_definition" { defaults = { arn = "arn:aws:ecs:eu-central-1:123456789012:task-definition/rise:1" } }
  mock_resource "aws_lb" { defaults = { arn = "arn:aws:elasticloadbalancing:eu-central-1:123456789012:loadbalancer/net/rise/1234567890123456" } }
  mock_resource "aws_lb_target_group" { defaults = { arn = "arn:aws:elasticloadbalancing:eu-central-1:123456789012:targetgroup/rise/1234567890123456" } }
}
mock_provider "random" {}
"""


def run(args, cwd, **kwargs):
    return subprocess.run(args, cwd=cwd, check=True, timeout=240, **kwargs)


def prepare(repo, work, baseline):
    with (work / "baseline.tar").open("wb") as archive:
        run(["git", "archive", baseline, *PATHS], repo, stdout=archive)
    with tarfile.open(work / "baseline.tar") as archive:
        archive.extractall(work / "baseline", filter="data")
    for relative in PATHS:
        source, target = repo / relative, work / "current" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(
                source,
                target,
                ignore=shutil.ignore_patterns(
                    ".terraform",
                    ".terraform.lock.hcl",
                    "terraform.tfstate*",
                    "*.tfvars",
                    "*.tfvars.json",
                ),
            )
        else:
            shutil.copy2(source, target)
    (work / "main.tf").write_text("""terraform {
  required_providers {
    aws = { source = "hashicorp/aws", version = ">= 6.50" }
    random = { source = "hashicorp/random", version = ">= 3.6" }
  }
}
""")
    module = repo / "modules/rise-ecs"
    shutil.copy2(module / ".terraform.lock.hcl", work / ".terraform.lock.hcl")
    shutil.copytree(
        module / ".terraform/providers", work / ".terraform/providers", symlinks=True
    )
    (work / "tests").mkdir()
    for case, relative in [
        (case, PATHS[1] if case == "e2e" else PATHS[0])
        for case in ["nlb", "alb", "brought", "dex", "endpoints", "e2e"]
    ]:
        source = (work / "baseline" / relative / "tests/plan.tftest.hcl").read_text()
        variables = re.search(
            r"^variables \{.*?^\}", source, re.MULTILINE | re.DOTALL
        ).group()
        overrides = ""
        if case == "e2e":
            overrides = re.search(
                r"^override_data \{\n  target = data.terraform_remote_state.bootstrap.*?^\}",
                source,
                re.MULTILINE | re.DOTALL,
            ).group()
        if case in {"alb", "endpoints"}:
            variables = re.sub(
                r'acme_email\s*= "ops@example.com"',
                '''acme_email = null
  edge_mode = "alb-acm"
  acm_certificate_arn = "arn:aws:acm:eu-central-1:123456789012:certificate/12345678-1234-1234-1234-123456789012"''',
                variables,
            )
        if case == "brought":
            variables = (
                variables[:-1]
                + """
  vpc = { id = "vpc-0123456789abcdef0", public_subnet_ids = ["subnet-a", "subnet-b"], private_subnet_ids = ["subnet-c", "subnet-d"] }
  cluster = { name = "existing" }
  cloud_map_namespace_id = "ns-existing"
  cloud_map_namespace_name = "existing.internal"
  database_url_secret_arn = "arn:aws:secretsmanager:eu-central-1:123456789012:secret:existing-db-abc123"
  create_traefik_task_role = false
  traefik_task_role_arn = "arn:aws:iam::123456789012:role/existing-traefik"
}"""
            )
        if case == "dex":
            variables = (
                variables[:-1]
                + """
  deploy_dex = true
  dex_admin_password_bcrypt = "$2y$10$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  nat_gateway_mode = "per_az"
  route53_zone_id = "Z0123456789"
}"""
            )
        if case == "endpoints":
            variables = (
                variables[:-1]
                + """
  nat_gateway_mode = "none"
  enable_vpc_endpoints = true
}"""
            )
        runs = ""
        for phase, command in [("baseline", "apply"), ("current", "plan")]:
            runs += f'''run "{phase}" {{
  command = {command}
  state_key = "{case}"
  module {{ source = "./{phase}/{relative}" }}
}}
'''
        (work / "tests" / f"{case}.tftest.hcl").write_text(
            MOCKS + overrides + "\n" + variables + "\n" + runs,
        )


def verify(log, repo):
    checked = set()
    destinations_by_path = {
        path: dict(
            re.findall(
                r"from\s*=\s*(\S+)\s+to\s*=\s*(\S+)",
                (repo / path / "moved.tf").read_text(),
            )
        )
        for path in PATHS[:2]
    }
    baseline_resources = {}
    e2e_fields = {
        "module.runtime.aws_ecs_task_definition.rise": {"container_definitions"},
        "module.runtime.aws_ecs_task_definition.traefik": {"container_definitions"},
        "module.runtime.aws_ecs_service.rise": {"enable_execute_command"},
        "module.runtime.aws_ecs_service.traefik": {
            "deployment_minimum_healthy_percent",
            "deployment_maximum_percent",
            "propagate_tags",
        },
    }
    # The baseline hashes each secret version to 60 bits, more than the
    # provider carries exactly; the current 52-bit hash writes every version
    # once more, with the same value. Production cases only: the E2E workspace
    # passes its secrets as plain environment.
    secret_fields = {
        f"module.{address}": {"secret_string_wo_version"}
        for address in [
            "database.aws_secretsmanager_secret_version.database_url[0]",
            "secrets.aws_secretsmanager_secret_version.encryption_key",
            "secrets.aws_secretsmanager_secret_version.jwt_signing_secret",
            "secrets.aws_secretsmanager_secret_version.oidc_client_secret",
        ]
    }
    for line in log.read_text().splitlines():
        item = json.loads(line)
        if (
            item.get("type") == "diagnostic"
            and item["diagnostic"]["severity"] == "error"
        ):
            raise AssertionError(item["diagnostic"])
        case = Path(item.get("@testfile", "")).name.split(".")[0]
        if item.get("type") == "test_state" and item.get("@testrun") == "baseline":

            def resources(module):
                return [
                    r for r in module.get("resources", []) if r["mode"] == "managed"
                ] + [
                    r
                    for child in module.get("child_modules", [])
                    for r in resources(child)
                ]

            baseline_resources[case] = resources(item["test_state"]["root_module"])
        if item.get("type") != "test_plan" or item.get("@testrun") != "current":
            continue
        changes = item["test_plan"]["resource_changes"]
        moves = {
            c["address"]: c["previous_address"]
            for c in changes
            if "previous_address" in c
        }
        destinations = destinations_by_path[PATHS[1] if case == "e2e" else PATHS[0]]
        expected_moves = {}
        for resource in baseline_resources[case]:
            address = resource["address"]
            base = address.split("[", 1)[0]
            assert (
                case == "e2e" or base in destinations or base.startswith("module.")
            ), (case, "unmapped resource", address)
            if base in destinations:
                target = destinations[base] + address[len(base) :]
                expected_moves[target] = address
        assert moves == expected_moves, (
            case,
            "incomplete state moves",
            moves,
            expected_moves,
        )
        for change in changes:
            address, delta = change["address"], change["change"]
            if delta["actions"] == ["no-op"]:
                continue
            allowed = e2e_fields if case == "e2e" else secret_fields
            assert address in allowed, (case, address, delta["actions"])
            assert delta["actions"] == ["update"], (case, address, delta["actions"])
            before, after = delta["before"], delta["after"]
            fields = {
                key
                for key in before.keys() | after.keys()
                if before.get(key) != after.get(key)
            }
            assert fields <= allowed[address], (case, address, fields)
        for name, delta in item["test_plan"].get("output_changes", {}).items():
            assert delta["actions"] == ["no-op"], (case, "changed output", name)
        checked.add(case)
        print(
            f"{case}: {len(moves)} state moves; "
            + (
                "only expected E2E task/service updates"
                if case == "e2e"
                else "only expected secret-version updates"
            )
        )
    assert checked == {"nlb", "alb", "brought", "dex", "endpoints", "e2e"}, (
        "missing upgrade plans",
        checked,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-ref", default=BASELINE)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    terraform = shutil.which("terraform")
    if not terraform:
        parser.error("terraform must be on PATH")
    with tempfile.TemporaryDirectory(prefix="rise-ecs-upgrade-") as directory:
        work = Path(directory)
        prepare(repo, work, args.baseline_ref)
        run(
            [terraform, "init", "-backend=false", "-input=false", "-no-color"],
            work,
            stdout=subprocess.DEVNULL,
        )
        log = work / "results.jsonl"
        with log.open("w") as output:
            result = subprocess.run(
                [terraform, "test", "-json", "-verbose"],
                cwd=work,
                stdout=output,
                stderr=subprocess.STDOUT,
                timeout=240,
                check=False,
            )
        try:
            verify(log, repo)
        except (AssertionError, KeyError):
            with tempfile.NamedTemporaryFile(
                prefix="rise-ecs-upgrade-failure-", suffix=".jsonl", delete=False
            ) as saved:
                saved.write(log.read_bytes())
            print(f"Terraform diagnostics: {saved.name}")
            raise
        assert result.returncode == 0, "Terraform upgrade test failed"


if __name__ == "__main__":
    main()
