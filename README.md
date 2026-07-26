# Bender

<img src="logo.png" alt="Bender logo" width="380">

Install Bender once, enter any project folder and run `bender`. Bender uses that folder as its security boundary, receives development tasks locally or over Nostr, delegates coding work to Codex CLI or an optional local worker, independently runs approved checks, and reports completion evidence.

Bender controls the job. Codex performs the main coding work. A model response or zero Codex exit status never marks a job complete.

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/lnbits/bender/main/install.sh | sh

codex login

cd ~/Projects/some-project
bender setup
# Review the proposal, then approve its argv arrays:
bender setup --accept-detected
bender doctor
bender
```

Bare `bender` is the normal equivalent of `bender run`. It canonicalizes the current directory and never switches to Bender's installation directory. The local UI is printed at startup and binds to `127.0.0.1:7331` by default.

VS Code is optional: run the same commands in its integrated terminal or any normal terminal. No extension is required.

## One folder, one Bender

If Bender starts in `/srv/projects/example`, that directory is the immutable workspace root. Project source scans do not follow symlinks. Reads and new write targets are checked through canonical existing ancestors; parent traversal, absolute external paths, sibling repositories, symlink escapes, `.git`, and worker writes to `.bender` are rejected.

Each project owns:

```text
.bender/
├── config.toml
├── project.toml
├── jobs/
└── artifacts/
```

Codex authentication stays under Codex's own machine-level login. Bender neither copies nor stores it. Ollama model files also remain managed by Ollama.

Useful commands:

```text
bender
bender run
bender init
bender setup
bender doctor
bender jobs
bender models
bender workers
bender status
bender version
bender update
```

## Approved project commands

`bender setup` only prints detected proposals. Nothing is approved until the project owner edits `.bender/project.toml` or runs `bender setup --accept-detected`.

Commands are argv arrays—Bender never builds an implicit `sh -c` string:

```toml
[project]
name = "example"

[commands]
setup = ["uv", "sync"]
build = ["npm", "run", "build"]
lint = ["uv", "run", "ruff", "check", "."]
typecheck = ["npm", "run", "typecheck"]
unit = ["uv", "run", "pytest", "-q"]
integration = ["uv", "run", "pytest", "tests/integration"]
start = ["uv", "run", "app"]
ui = ["npx", "playwright", "test"]

[completion]
required_checks = ["lint", "typecheck", "unit"]
max_attempts = 4
require_approval = true
require_review = false
```

Checks run with the workspace as their current directory, bounded output and timeouts, a minimal environment, no inherited SSH agent, and process-group cleanup. `sudo`, Docker, Git push, deployment, and arbitrary commands are not completion checks.

Bender tells Codex to inspect `AGENTS.md` and `.bender/instructions.md` when present; their project-specific content is not built into Bender.

## Job lifecycle and evidence

A local or Nostr task becomes:

```text
Received → Clarifying → AwaitingApproval → Approved
→ Working → Checking → Fixing (when needed)
→ Reviewing (optional) → Complete
```

Other terminal/action states are `AwaitingActionApproval`, `Blocked`, `Failed`, and `Cancelled`. Each job is stored atomically under `.bender/jobs/<job-id>/` with the request, conversation, approved specification, criteria, events, worker invocations, changed files, check results, review, completion gates, artifacts, and final report.

On restart, an in-flight job is marked interrupted and blocked for explicit resume/retry; Bender never assumes an old PID is alive or completes it automatically.

The repair loop is:

```text
Codex implementation
→ Bender runs approved checks
→ exact failure evidence returns to the Codex session
→ Codex repairs
→ Bender reruns checks
```

Repeated unchanged failures and maximum attempts block the job. “Not run” is not “passed,” and all configured required gates must pass.

## Codex, Qwen and Gemma

Codex CLI is the primary worker. Bender invokes the installed `codex exec` using argv, `--json`, a JSON output schema, `workspace-write`, an explicit approval policy, and the selected folder as `--cd`. Stdout/stderr and process metadata are retained. Hidden chain-of-thought is neither requested nor stored.

Qwen through Ollama is an optional fallback and is never selected silently:

```toml
[workers.qwen]
enabled = true
provider = "ollama"
base_url = "http://127.0.0.1:11434"
model = "qwen2.5-coder:14b"
timeout_seconds = 1800
```

Gemma can independently review the approved request, diff/check evidence, and warnings:

```toml
[reviewers.gemma]
enabled = true
provider = "ollama"
base_url = "http://127.0.0.1:11434"
model = "gemma3:12b"
timeout_seconds = 600

[completion]
required_checks = ["unit"]
require_review = true
```

A reviewer may approve, request changes, or block. It cannot set `Complete`; Bender evaluates the gates.

Optional model installation remains a separate user action:

```bash
ollama pull qwen2.5-coder:14b
ollama pull gemma3:12b
```

## Nostr remote control

Configure exactly one controller identity in the loopback setup UI. Bender ignores other senders. A task is persisted with its conversation identifier and acceptance criteria; the controller must reply `APPROVE` before Codex or checks run. Bender returns the terminal state and final report without sending verbose raw worker logs unless requested.

The web UI is not required for Nostr-only operation, and it is not exposed publicly by default.

## VPS service

The architecture is identical on a headless server:

```bash
codex login
cd /srv/projects/example
bender setup
bender doctor
bender
```

For persistence, copy `packaging/bender.service.example` to
`~/.config/systemd/user/bender-example.service`, edit both absolute paths, then:

```bash
systemctl --user daemon-reload
systemctl --user enable --now bender-example
loginctl enable-linger "$USER"   # optional; administrator policy may apply
```

The service sets an explicit `WorkingDirectory` and loopback bind. Run one service—with a distinct port and state directory—for each explicit workspace. Do not create one daemon with access to every repository. Nostr is the intended remote channel.

## Runtime and browser checks

Projects may define an approved supervised runtime and UI category:

```toml
[runtime]
start_command = "start"
base_url = "http://127.0.0.1:5000"
healthcheck_url = "http://127.0.0.1:5000/health"
startup_timeout_seconds = 90

[ui]
enabled = true
test_command = "ui"
browser = "chromium"
fail_on_console_error = true
```

Playwright, browsers, Node, Python, Rust, Docker, Ollama, and Codex are not installed by Bender. `bender doctor` checks only ecosystems relevant to the approved project configuration.

## Development and release

```bash
nix develop
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo build --release --locked
```

Release tags drive `.github/workflows/release.yml`. A maintainer reviews the clean diff and CI, updates the version/changelog, commits, creates an annotated `vX.Y.Z` tag, and pushes that tag. Bender never pushes, tags, publishes, deploys, or releases without explicit authorization.

Skills and thin editor clients can be added later; neither expands the workspace boundary nor overrides completion gates.
