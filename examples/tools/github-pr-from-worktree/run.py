#!/usr/bin/env python3
import json
import re
import shutil
import subprocess
import sys
from datetime import datetime, timezone


PROTECTED_DIRS = (".bender/", ".git/", "target/", "node_modules/")
PROTECTED_FILES = (".bender", ".git", "target", "node_modules")
PROTECTED_BRANCHES = {"main", "master", "develop", "dev"}


def respond(payload, exit_code=0):
    print(json.dumps(payload, separators=(",", ":")))
    raise SystemExit(exit_code)


def run(args):
    completed = subprocess.run(args, text=True, capture_output=True)
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        respond({"ok": False, "error": detail or f"{args[0]} failed", "command": args}, 1)
    return completed.stdout.strip()


def run_raw(args):
    completed = subprocess.run(args, text=True, capture_output=True)
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        respond({"ok": False, "error": detail or f"{args[0]} failed", "command": args}, 1)
    return completed.stdout


def require_string(data, key):
    value = data.get(key)
    if not isinstance(value, str) or not value.strip():
        respond({
            "ok": False,
            "error": f"missing `{key}`",
            "guidance": f"Provide `{key}` or omit it and let this tool infer a default.",
        }, 1)
    return value.strip()


def optional_string(data, key):
    value = data.get(key)
    if value is None:
        return None
    if not isinstance(value, str):
        respond({"ok": False, "error": f"{key} must be a string"}, 1)
    value = value.strip()
    return value or None


def require_bool(data, key, default):
    value = data.get(key, default)
    if not isinstance(value, bool):
        respond({"ok": False, "error": f"{key} must be a boolean"}, 1)
    return value


def validate_branch(branch):
    if branch in PROTECTED_BRANCHES:
        respond({"ok": False, "error": f"refusing to work directly on protected branch {branch}"}, 1)
    if branch.startswith("-") or ".." in branch or branch.endswith(".lock"):
        respond({"ok": False, "error": "unsafe branch name"}, 1)
    if not re.fullmatch(r"[A-Za-z0-9._/-]{1,120}", branch):
        respond({"ok": False, "error": "branch contains unsupported characters"}, 1)


def changed_paths():
    output = run_raw(["git", "status", "--porcelain=v1"])
    paths = []
    for line in output.splitlines():
        if not line:
            continue
        raw = line[3:]
        if " -> " in raw:
            raw = raw.split(" -> ", 1)[1]
        paths.append(raw.strip())
    return paths


def slugify(value):
    slug = re.sub(r"[^A-Za-z0-9]+", "-", value.lower()).strip("-")
    slug = re.sub(r"-{2,}", "-", slug)
    return slug[:60].strip("-") or "update-files"


def describe_paths(paths):
    if not paths:
        return "Update files"
    first = paths[0]
    stem = first.rsplit("/", 1)[-1]
    if len(paths) == 1:
        return f"Update {stem}"
    return f"Update {stem} and {len(paths) - 1} more file{'s' if len(paths) != 2 else ''}"


def infer_values(data, paths):
    intent = optional_string(data, "intent")
    summary = optional_string(data, "summary")
    if summary is None and intent and intent != "create_pr":
        summary = intent
    description = describe_paths(paths)
    title = optional_string(data, "title") or summary or description
    commit_message = optional_string(data, "commit_message") or title or "Update files"
    body = optional_string(data, "body") or f"{title}\n\nChanged files:\n" + "\n".join(f"- {path}" for path in paths)
    branch = optional_string(data, "branch")
    if branch is None:
        date = datetime.now(timezone.utc).strftime("%Y%m%d")
        branch = f"bender/{date}-{slugify(title)}"
    return branch, commit_message, title, body


def ensure_paths_are_allowed(paths):
    for path in paths:
        if path in PROTECTED_FILES or path.startswith(PROTECTED_DIRS):
            respond({"ok": False, "error": f"refusing to commit protected path: {path}"}, 1)


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError as exc:
        respond({"ok": False, "error": f"invalid JSON input: {exc}"}, 1)

    base = data.get("base") or "main"
    draft = require_bool(data, "draft", True)
    dry_run = require_bool(data, "dry_run", False)

    if not isinstance(base, str) or not base.strip():
        respond({"ok": False, "error": "base must be a non-empty string"}, 1)
    base = base.strip()
    if shutil.which("git") is None:
        respond({"ok": False, "error": "git is not installed or not on PATH"}, 1)

    run(["git", "rev-parse", "--is-inside-work-tree"])

    paths = changed_paths()
    if not paths:
        respond({"ok": False, "error": "no worktree changes to commit"}, 1)
    ensure_paths_are_allowed(paths)

    branch, commit_message, title, body = infer_values(data, paths)
    validate_branch(branch)

    pr_command = ["gh", "pr", "create", "--title", title, "--body", body, "--base", base]
    if draft:
        pr_command.append("--draft")

    commands = [
        ["git", "checkout", "-B", branch],
        ["git", "add", "--", *paths],
        ["git", "commit", "-m", commit_message],
        ["git", "push", "-u", "origin", branch],
        pr_command,
    ]
    if dry_run:
        respond({
            "ok": True,
            "dry_run": True,
            "branch": branch,
            "commit_message": commit_message,
            "title": title,
            "body": body,
            "changed_paths": paths,
            "commands": commands,
        })

    if shutil.which("gh") is None:
        respond({"ok": False, "error": "GitHub CLI is not installed or not on PATH"}, 1)
    run(["gh", "auth", "status"])

    run(["git", "checkout", "-B", branch])
    run(["git", "add", "--", *paths])
    run(["git", "commit", "-m", commit_message])
    run(["git", "push", "-u", "origin", branch])
    url = run(pr_command)

    respond({
        "ok": True,
        "branch": branch,
        "commit_message": commit_message,
        "changed_paths": paths,
        "url": url,
        "draft": draft,
    })


if __name__ == "__main__":
    main()
