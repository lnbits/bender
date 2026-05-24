#!/usr/bin/env python3
import json
import shutil
import subprocess
import sys


def respond(payload, exit_code=0):
    print(json.dumps(payload, separators=(",", ":")))
    raise SystemExit(exit_code)


def run(args):
    completed = subprocess.run(args, text=True, capture_output=True)
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        respond({"ok": False, "error": detail or f"{args[0]} failed"}, 1)
    return completed.stdout.strip()


def require_string(data, key):
    value = data.get(key)
    if not isinstance(value, str) or not value.strip():
        respond({"ok": False, "error": f"missing required string: {key}"}, 1)
    return value.strip()


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError as exc:
        respond({"ok": False, "error": f"invalid JSON input: {exc}"}, 1)

    title = require_string(data, "title")
    body = require_string(data, "body")
    base = data.get("base") or "main"
    draft = data.get("draft", True)
    push = data.get("push", False)
    dry_run = data.get("dry_run", False)

    if not isinstance(base, str) or not base.strip():
        respond({"ok": False, "error": "base must be a non-empty string"}, 1)
    if not isinstance(draft, bool):
        respond({"ok": False, "error": "draft must be a boolean"}, 1)
    if not isinstance(push, bool):
        respond({"ok": False, "error": "push must be a boolean"}, 1)
    if not isinstance(dry_run, bool):
        respond({"ok": False, "error": "dry_run must be a boolean"}, 1)

    pr_command = ["gh", "pr", "create", "--title", title, "--body", body, "--base", base.strip()]
    if draft:
        pr_command.append("--draft")

    if dry_run:
        commands = []
        if push:
            commands.append(["git", "push", "-u", "origin", "<current-branch>"])
        commands.append(pr_command)
        respond({"ok": True, "dry_run": True, "commands": commands})

    if shutil.which("git") is None:
        respond({"ok": False, "error": "git is not installed or not on PATH"}, 1)
    if shutil.which("gh") is None:
        respond({"ok": False, "error": "GitHub CLI is not installed or not on PATH"}, 1)

    run(["git", "rev-parse", "--is-inside-work-tree"])
    branch = run(["git", "branch", "--show-current"])
    if not branch:
        respond({"ok": False, "error": "could not determine current git branch"}, 1)
    if branch in {"main", "master"}:
        respond({"ok": False, "error": f"refusing to create a PR directly from {branch}"}, 1)

    status = run(["git", "status", "--porcelain"])
    if status:
        respond({"ok": False, "error": "working tree has uncommitted changes"}, 1)

    if push:
        run(["git", "push", "-u", "origin", branch])

    url = run(pr_command)
    respond({"ok": True, "url": url, "branch": branch, "draft": draft})


if __name__ == "__main__":
    main()
