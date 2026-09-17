#!/usr/bin/env python3
"""Bootstrap a dark-factory repository profile (ADR-0069).

Applies a declarative JSON profile (scripts/profiles/*.json) to the
dark-factory configuration surface: FactoryConfig today, FactoryRepo once
ADR-0069 is implemented. Idempotent: find-or-create by factory_id/repo_url,
dispatch Update with the full profile, then read back and verify.

Usage:
    python3 bootstrap_factory_repo.py <profile> [--dry-run]
    python3 bootstrap_factory_repo.py dark-factory-e2e
    python3 bootstrap_factory_repo.py den --base-url http://localhost:3100

<profile> is a name in scripts/profiles/ or a path to a JSON file.

Auth: logs in through /auth/login like the console (ADR-0067 D2 — no BFF).
Defaults are the throwaway local e2e account; override with
FACTORY_EMAIL / FACTORY_PASSWORD env vars or --email/--password.
"""

from __future__ import annotations

import argparse
import hashlib
import http.cookiejar
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

PROFILES_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "profiles")

# FactoryConfig.Update params (specs/factory_config.ioa.toml,
# strict_action_params = true — every declared param must be sent).
UPDATE_PARAMS = [
    "repo_url",
    "base_branch",
    "checkout_mode",
    "publish_mode",
    "pi_provider",
    "pi_model",
    "model_env_var",
    "test_commands",
    "lint_commands",
    "observation_commands",
    "computer_cpu_cores",
    "computer_memory_gb",
    "computer_storage_gb",
    "computer_provider",
    "computer_image",
    "setup_script",
    "max_repair_rounds",
    "max_files_per_task",
    "max_lines_per_task",
]

# FactoryRepo Revise/Update profile params (specs/factory_repo.ioa.toml,
# strict_action_params = true). profile_digest and updated_by are computed
# per write and appended separately.
REPO_PARAMS = [
    "display_name",
    "description",
    "team_id",
    "git_provider",
    "git_url",
    "base_branch",
    "checkout_mode",
    "publish_mode",
    "publish_credential_ref",
    "validation_commands",
    "build_commands",
    "deploy_commands",
    "observation_commands",
    "preparation_commands",
    "computer_image",
    "setup_script",
    "cpu_cores",
    "memory_gb",
    "storage_gb",
    "source_credential_ref",
    "command_secret_bindings",
]

# ADR-0069 single digest rule: sha256 over sort-keyed compact JSON of the
# profile param fields + profile_revision. Mirrors
# wasm/factory-common/src/lib.rs repo_profile_digest_payload.
def repo_digest(fields: dict, revision: int) -> str:
    payload = {k: str(fields.get(k, "")) for k in REPO_PARAMS}
    payload["profile_revision"] = str(revision)
    canon = json.dumps(payload, sort_keys=True, separators=(",", ":"))
    return "sha256:" + hashlib.sha256(canon.encode()).hexdigest()


class Client:
    def __init__(self, base_url: str, tenant: str) -> None:
        self.base_url = base_url.rstrip("/")
        self.tenant = tenant
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar())
        )

    def request(self, path: str, body: dict | None = None) -> dict:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method="POST" if body is not None else "GET")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with self.opener.open(req) as resp:
                return json.loads(resp.read().decode() or "{}")
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode()[:400]
            raise SystemExit(f"HTTP {exc.code} {url}: {detail}")

    def q(self, path: str) -> str:
        sep = "&" if "?" in path else "?"
        return f"{path}{sep}tenant={urllib.parse.quote(self.tenant)}"

    def login(self, email: str, password: str) -> None:
        self.request("/auth/login", {"email": email, "password": password})

    def list_configs(self) -> list[dict]:
        return self.request(self.q("/tdata/FactoryConfigs")).get("value", [])

    def create_config(self, factory_id: str) -> str:
        created = self.request(self.q("/tdata/FactoryConfigs"), {"factory_id": factory_id})
        return created["entity_id"]

    def update_config(self, entity_id: str, params: dict) -> None:
        path = f"/tdata/FactoryConfigs('{entity_id}')/Temper.DarkFactory.Update"
        self.request(self.q(path), params)

    def get_config(self, entity_id: str) -> dict:
        return self.request(self.q(f"/tdata/FactoryConfigs('{entity_id}')"))

    def list_repos(self) -> list[dict]:
        return self.request(self.q("/tdata/FactoryRepos")).get("value", [])

    def create_repo(self, repo_id: str, display_name: str, created_by: str) -> str:
        created = self.request(
            self.q("/tdata/FactoryRepos"),
            {"repo_id": repo_id, "display_name": display_name, "created_by": created_by},
        )
        return created["entity_id"]

    def dispatch_repo(self, entity_id: str, action: str, params: dict | None = None) -> None:
        path = f"/tdata/FactoryRepos('{entity_id}')/Temper.DarkFactory.{action}"
        self.request(self.q(path), params or {})

    def get_repo(self, entity_id: str) -> dict:
        return self.request(self.q(f"/tdata/FactoryRepos('{entity_id}')"))


def fields_of(row: dict) -> dict:
    return row.get("fields") or {}


def poll_until(fn, timeout: float = 20.0):
    """Query projections trail dispatches by a beat — poll until they catch up."""
    deadline = time.time() + timeout
    while True:
        result = fn()
        if result is not None:
            return result
        if time.time() >= deadline:
            return None
        time.sleep(1)


def seed_repo(client: Client, name: str, profile: dict, email: str, dry_run: bool) -> None:
    """Find-or-create a FactoryRepo, Revise/Update with digest, Activate."""
    section = profile["repo"]
    target = {k: str(section.get(k, "")) for k in REPO_PARAMS}
    if not target["git_url"]:
        raise SystemExit("profile repo section is missing git_url")
    if not target["display_name"]:
        target["display_name"] = name

    rows = client.list_repos()
    existing = next(
        (r for r in rows if fields_of(r).get("repo_id") == name),
        None,
    )
    if existing is None:
        candidates = [
            r
            for r in rows
            if fields_of(r).get("git_url") == target["git_url"]
            and fields_of(r).get("Status") != "Archived"
        ]
        if len(candidates) > 1:
            raise SystemExit(
                f"multiple non-Archived repos match {target['git_url']}: "
                f"{[r.get('entity_id') for r in candidates]} — set repo_id to disambiguate"
            )
        existing = candidates[0] if candidates else None

    if existing is None:
        entity_id = None
        status = "(new)"
        current = {}
    else:
        entity_id = existing.get("entity_id") or fields_of(existing).get("Id")
        current = fields_of(existing)
        status = current.get("Status", "?")
        if status == "Archived":
            raise SystemExit(f"repo {entity_id} is Archived — create a new profile instead of reviving it")

    try:
        current_revision = int(str(current.get("profile_revision") or "0"))
    except ValueError:
        current_revision = 0
    next_revision = current_revision + 1
    digest = repo_digest(target, next_revision)
    params = dict(target)
    params["profile_digest"] = digest
    params["updated_by"] = email

    drift = sorted(k for k in REPO_PARAMS if str(current.get(k, "")) != target[k])
    # The digest covers the revision, so compare the stored digest against a
    # recomputation at the row's CURRENT revision — not next_revision's.
    in_sync = not drift and current.get("profile_digest") == repo_digest(
        {k: str(current.get(k, "")) for k in REPO_PARAMS}, current_revision
    )
    action = {"Draft": "Revise", "Active": "Update"}.get(status)
    plan = (
        "create → Revise → Activate" if entity_id is None
        else "no-op (already in sync)" if in_sync
        else f"{action} (drift: {', '.join(drift) if drift else 'profile_digest'})"
    )
    if status == "Draft" and in_sync:
        plan = "Activate (content already in sync)"
    print(f"repo    : {entity_id or '(new)'} Status={status} rev={current_revision}")
    print(f"plan    : {plan}")

    if dry_run:
        return

    if entity_id is None:
        entity_id = client.create_repo(name, target["display_name"], email)
        print(f"created : {entity_id}")
        status = "Draft"
        action = "Revise"
        in_sync = False

    if in_sync:
        print("repo    : profile already in sync — skipping write")
    else:
        client.dispatch_repo(entity_id, action, params)

        def verified():
            row = fields_of(client.get_repo(entity_id))
            bad = [k for k in REPO_PARAMS if str(row.get(k, "")) != target[k]]
            if row.get("profile_digest") != digest:
                bad.append("profile_digest")
            if str(row.get("profile_revision")) != str(next_revision):
                bad.append(f"profile_revision={row.get('profile_revision')}")
            return row if not bad else None

        if poll_until(verified) is None:
            raise SystemExit("VERIFY FAILED — FactoryRepo fields differ after write")
        print(f"verified: rev={next_revision} digest={digest[:23]}…")

    if status == "Draft":
        client.dispatch_repo(entity_id, "Activate")
        row = poll_until(lambda: (lambda r: r if r.get("Status") == "Active" else None)(fields_of(client.get_repo(entity_id))))
        if row is None:
            raise SystemExit("VERIFY FAILED — FactoryRepo did not reach Active")
        print("activated: Status=Active")


def load_profile(arg: str) -> dict:
    path = arg if os.path.isfile(arg) else os.path.join(PROFILES_DIR, f"{arg}.json")
    if not os.path.isfile(path):
        raise SystemExit(f"profile not found: {arg} (looked in {PROFILES_DIR})")
    with open(path, encoding="utf-8") as fh:
        profile = json.load(fh)
    if "config" in profile:
        missing = [k for k in UPDATE_PARAMS if k not in profile["config"]]
        if missing:
            raise SystemExit(f"profile {path} is missing config keys: {', '.join(missing)}")
    if "config" not in profile and "repo" not in profile:
        raise SystemExit(f"profile {path} has neither a 'config' nor a 'repo' section")
    return profile


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("profile", help="profile name in scripts/profiles/ or path to a JSON file")
    parser.add_argument("--base-url", default=os.environ.get("TEMPER_URL", "http://localhost:3100"))
    parser.add_argument("--tenant", default=os.environ.get("TEMPER_TENANT", "default"))
    parser.add_argument("--email", default=os.environ.get("FACTORY_EMAIL", "e2e@darkfactory.local"))
    parser.add_argument("--password", default=os.environ.get("FACTORY_PASSWORD", "e2e-factory-pass"))
    parser.add_argument("--dry-run", action="store_true", help="resolve and diff only; create/update nothing")
    args = parser.parse_args()

    profile = load_profile(args.profile)
    name = profile["name"]
    if "repo" in profile:
        seed_repo_only = profile.get("config") is None
        if seed_repo_only:
            client = Client(args.base_url, args.tenant)
            client.login(args.email, args.password)
            print(f"profile : {name} — {profile.get('description', '')}")
            print(f"target  : {args.base_url} tenant={args.tenant}")
            seed_repo(client, name, profile, args.email, args.dry_run)
            print("OK")
            return
    config = {k: profile["config"][k] for k in UPDATE_PARAMS}

    client = Client(args.base_url, args.tenant)
    client.login(args.email, args.password)

    rows = client.list_configs()
    # factory_id is the primary key when set; the repo_url fallback must only
    # consider Active rows — Archived configs are historical iterations of
    # the same repo and must never be revived by a bootstrap.
    existing = next(
        (row for row in rows if fields_of(row).get("factory_id") == name),
        None,
    )
    if existing is None:
        active = [
            row
            for row in rows
            if fields_of(row).get("repo_url") == config["repo_url"]
            and fields_of(row).get("Status") == "Active"
        ]
        if len(active) > 1:
            raise SystemExit(
                "multiple Active configs match "
                f"{config['repo_url']}: {[r.get('entity_id') for r in active]} — set factory_id to disambiguate"
            )
        existing = active[0] if active else None

    if existing is None:
        entity_id = None
        action = "create+update"
    else:
        entity_id = existing.get("entity_id") or fields_of(existing).get("Id")
        current = fields_of(existing)
        drift = sorted(k for k in UPDATE_PARAMS if str(current.get(k, "")) != str(config[k]))
        action = f"update (drift: {', '.join(drift)})" if drift else "no-op (already in sync)"

    print(f"profile : {name} — {profile.get('description', '')}")
    print(f"target  : {args.base_url} tenant={args.tenant}")
    print(f"entity  : {entity_id or '(new)'}")
    print(f"plan    : {action}")

    if args.dry_run:
        print("dry-run  : no changes made")
        if "repo" in profile:
            seed_repo(client, name, profile, args.email, args.dry_run)
        return

    if entity_id is None:
        entity_id = client.create_config(name)
        print(f"created : {entity_id}")

    client.update_config(entity_id, config)

    # Read-back verification: the entity must hold exactly what we sent. The
    # query projection trails the dispatch by a beat — poll until it catches up.
    mismatches = UPDATE_PARAMS
    verified = {}
    deadline = time.time() + 20
    while time.time() < deadline:
        verified = fields_of(client.get_config(entity_id))
        mismatches = [k for k in UPDATE_PARAMS if str(verified.get(k, "")) != str(config[k])]
        if not mismatches:
            break
        time.sleep(1)
    if mismatches:
        raise SystemExit(f"VERIFY FAILED — fields differ after update: {', '.join(mismatches)}")
    status = verified.get("Status", "?")
    print(f"verified: {entity_id} Status={status} repo_url={verified.get('repo_url')}")
    print(f"          tests={verified.get('test_commands')}")
    if "repo" in profile:
        seed_repo(client, name, profile, args.email, args.dry_run)
    print("OK")


if __name__ == "__main__":
    main()
