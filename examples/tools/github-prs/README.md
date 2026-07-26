# GitHub PRs Tool

This drop-in Bender tool handles small GitHub branch and PR workflows.

Actions:

- `pull`: fetch/prune remote branches, fast-forward the current branch when it
  has an upstream, and return new/updated remote branches with latest commit
  subjects.
- `switch_branch`: checkout an existing branch, for example `main`.
- `create_pr`: create/reset a feature branch, stage current worktree changes,
  commit them, push, and create a draft GitHub PR.

It expects:

- `python3`
- `git`
- a git repository with a remote for `pull`
- GitHub CLI `gh` for `create_pr`
- an authenticated `gh` session for `create_pr`
- a git repository with a GitHub remote for `create_pr`

Expected drop-in shape:

```text
/some/trusted/tools/github-prs/
  bender-tool.toml
  run.py
```

Switch branch dry run:

```sh
printf '%s\n' '{"action":"switch_branch","branch":"main","dry_run":true}' | ./run.py
```

Pull dry run:

```sh
printf '%s\n' '{"action":"pull","dry_run":true}' | ./run.py
```

Pull output includes `new_remote_branches` and `updated_remote_branches`. Each
entry includes the remote branch name, short commit hash, and latest commit
subject so Bender can tell you what changed.

Minimal PR dry run:

```sh
printf '%s\n' '{"action":"create_pr","dry_run":true}' | ./run.py
```

The PR action infers `branch`, `commit_message`, `title`, and `body` from the
current worktree changes when they are omitted.

Detailed PR dry run:

```sh
printf '%s\n' '{
  "action": "create_pr",
  "branch": "bender/profile-links",
  "commit_message": "Open DM profiles from message list",
  "title": "Open DM profiles from message list",
  "body": "Adds profile navigation from DM profile clicks.",
  "dry_run": true
}' | ./run.py
```

The PR action refuses to commit protected paths such as `.bender`, `.git`,
`target`, and `node_modules`. It requires explicit Bender approval before it
runs.
