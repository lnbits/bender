# Hello Tool

This is a minimal demo of a drop-in Bender tool.

Expected shape:

```text
.bender/tools/hello/
  bender-tool.toml
  run.sh
```

Bender would execute `run.sh`, pass JSON on stdin, and read JSON from stdout.

Try it manually:

```sh
printf '{}\n' | ./run.sh
```

Expected output:

```json
{"ok":true,"message":"Hello from a drop-in Bender tool."}
```

This demo requests no permissions and does not require confirmation.

