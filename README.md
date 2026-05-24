
  <img src="logo.png" alt="Bender logo" width="300">

![Beta](https://img.shields.io/badge/status-beta-f0b429)

Clawbot’s model is backwards. It starts with access to everything and then tries to restrict permissions afterward. Bender takes the opposite approach: it only has access to a specific folder and the tools you explicitly give it, start with minimal access, then expand only when needed.

Run the Bender binary from any folder and he will only have access to that folder and what it contains. Connect over Nostr private DMs, because Nostr is good and Telegram and WhatsApp are also backwards.

```
# For bender to only see whats in some-other-project
cd /home/ben/Projects/some-other-project
/home/ben/Projects/bender/target/release/bender init
/home/ben/Projects/bender/target/release/bender run
```


  <img src="bender.gif" alt="Clawbot is dumb" width="300">


Bender is a tiny native Rust daemon.

The shape is:

```text
Nostr DM -> Bender daemon -> LLM provider -> Bender validates file changes -> changes are applied inside cwd
```

The model does not get shell access. It can answer normally, and when it needs to change files Bender applies validated changes inside the folder it was started from.

## Simple Install

Use one of the installers here: [latest release assets](../../releases/latest).

Release tags publish:

- `bender-linux-x86_64`: raw Linux binary.
- `bender-linux-x86_64.tar.gz`: compressed Linux binary.
- `bender-windows-x86_64.exe`: Windows executable.
- `bender-windows-x86_64.zip`: compressed Windows executable.
- `Bender-macos-x86_64.dmg`: macOS Intel DMG.
- `Bender-macos-aarch64.dmg`: macOS Apple Silicon DMG.
- `Bender-linux-x86_64.flatpak`: Flatpak bundle.

Drop the binary into the folder you want Bender to control:

```sh
./bender init
./bender run
```

Open:

```text
http://bender.localhost:7331
```

Bender binds to `127.0.0.1:7331` by default. If that port is busy:

```sh
./bender run --bind 127.0.0.1:7332
```

Or:

```sh
BENDER_BIND=127.0.0.1:7332 ./bender run
```

## Hard Install

Build from source:

```sh
nix develop path:/home/ben/Projects/watup
cargo build --release
```

The drop-in binary is:

```text
target/release/bender
```

Run from source while developing:

```sh
nix develop path:/home/ben/Projects/watup
cargo run -- init
cargo run -- run
```

With API keys from the environment:

```sh
OPENAI_API_KEY=sk-... cargo run -- run
ANTHROPIC_API_KEY=sk-ant-... cargo run -- run
DEEPSEEK_API_KEY=sk-... cargo run -- run
```

## Setup

The web UI lets you save:

- controller `npub`
- provider
- provider API keys
- local provider URLs
- model

## Drop-in tools

Bender can use extra tools only after you explicitly add their folder in the web
UI. Each tool is a folder with a `bender-tool.toml` manifest and an executable
that accepts JSON on stdin and returns JSON on stdout.

Example:

```text
/home/you/bender-tools/github-pr/
  bender-tool.toml
  run.py
```

Add that folder in the web UI under `Tool folder`. Tools are only available to
the Bender instance whose config lists that folder. A tool with
`requires_confirmation = true` will pause in the web UI and ask for approval
before it runs.

Bender core still validates file patches so they stay inside the folder Bender
is running in. Tools are separate executable code, so only add tool folders you
trust.

See `examples/tools/hello`, `examples/tools/github-pr`, and
`examples/tools/github-pr-from-worktree` for starter tools.

Use `github-pr` when the branch is already committed and pushed. Use
`github-pr-from-worktree` when you want the approved tool to create a branch,
commit the current changes, push, and open a draft PR.

Bender generates its own Nostr keypair during `bender init`. It stores config in:

```text
.bender/config.toml
```

Example:

```toml
name = "Bender"
secret_key = "nsec1..."
public_key = "npub1..."
controller_npub = ""
provider = "openai"
openai_api_key = ""
anthropic_api_key = ""
deepseek_api_key = ""
model = "gpt-5.1-codex-mini"
ollama_base_url = "http://127.0.0.1:11434"
llama_cpp_base_url = "http://127.0.0.1:8080"
bind = "127.0.0.1:7331"
relays = [
  "wss://relay.damus.io",
  "wss://nos.lol",
  "wss://relay.primal.net",
  "wss://relay.nostr.band",
  "wss://relay.snort.social",
  "wss://nostr.mom",
  "wss://offchain.pub",
  "wss://relay.current.fyi",
  "wss://nostr.wine",
  "wss://relayable.org",
]
```

Supported providers:

- `openai`: OpenAI and Codex models.
- `anthropic`: Claude API.
- `deepseek`: DeepSeek API.
- `ollama`: local Ollama models, no API key needed.
- `llama_cpp`: local llama.cpp OpenAI-compatible server, API key optional.

The model dropdown changes with the provider. Refresh Models asks the selected provider for available models when that provider has a model-list endpoint Bender can use.

## Nostr

Send a NIP-17 private DM to Bender's `npub` from the configured `controller_npub`:

```text
please add a notes file with ideas for the next release
```

Bender ignores DMs from any other sender. If your request is just a question, it replies with text. If your request needs a file change, it validates and applies it automatically.

Bender publishes its Nostr profile as `Bender`, with the bio `I bend things https://github.com/lnbits/bender`, `profile.png` as the picture, and `bender.gif` as the banner.

## Local URL

The portable no-setup URL is:

```text
http://bender.localhost:7331
```

`bender.local` needs either mDNS support or a hosts-file entry:

```text
127.0.0.1 bender.local
```

Then open:

```text
http://bender.local:7331
```

## Security Notes

Bender controls the folder you run it from. If you run `./bender init` inside `target/release`, it creates `target/release/.bender` and treats `target/release` as the project.

File changes are restricted to the current folder and Bender blocks writes into `.bender`, `.git`, `target`, and `node_modules`.

Hosted API usage is billed to the provider API key saved in Bender. Local Ollama and llama.cpp usage runs on your machine. Token use depends on the model, request size, and folder context sent with the request.
