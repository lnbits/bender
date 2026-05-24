# GitHub PR Tool

This is an example drop-in Bender tool that creates a GitHub pull request for
the current git branch.

It expects:

- `python3`
- `git`
- GitHub CLI `gh`
- an authenticated `gh` session
- a clean working tree with committed changes
- a feature branch, not `main` or `master`
- the branch already pushed to GitHub, unless `"push": true` is passed

Expected drop-in shape:

```text
.bender/tools/github-pr/
  bender-tool.toml
  run.py
```

Example dry run:

```sh
printf '%s\n' '{"title":"Demo PR","body":"Created by Bender.","dry_run":true}' | ./run.py
```

Example real run:

```sh
printf '%s\n' '{"title":"Demo PR","body":"Created by Bender.","base":"main","draft":true}' | ./run.py
```

The tool creates draft PRs by default and asks Bender to require confirmation
before use. It does not commit files. It also does not push by default; pass
`"push": true` only when you explicitly want it to push the current branch.
