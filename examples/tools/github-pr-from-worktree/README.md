# GitHub PR From Worktree Tool

This drop-in Bender tool handles the full GitHub PR workflow after Bender has
edited files:

1. create or reset a feature branch
2. stage current worktree changes
3. commit them
4. push the branch
5. create a draft GitHub PR

It expects:

- `python3`
- `git`
- GitHub CLI `gh`
- an authenticated `gh` session
- a git repository with a GitHub remote

Expected drop-in shape:

```text
/some/trusted/tools/github-pr-from-worktree/
  bender-tool.toml
  run.py
```

Minimal dry run:

```sh
printf '%s\n' '{"intent":"create_pr","dry_run":true}' | ./run.py
```

The tool infers `branch`, `commit_message`, `title`, and `body` from the current
worktree changes when they are omitted.

Detailed dry run:

```sh
printf '%s\n' '{
  "branch": "bender/profile-links",
  "commit_message": "Open DM profiles from message list",
  "title": "Open DM profiles from message list",
  "body": "Adds profile navigation from DM profile clicks.",
  "dry_run": true
}' | ./run.py
```

The tool refuses to commit protected paths such as `.bender`, `.git`, `target`,
and `node_modules`. It requires explicit Bender approval before it runs.
