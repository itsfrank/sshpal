# sshpal

`sshpal` is a Rust CLI for working with a remote Linux machine over SSH when that machine cannot fetch code or build tools from the internet.

It provides two main features:

- project-aware `push` / `pull` commands built on `rsync`
- a local RPC server so the remote machine can trigger local-only tasks such as macOS-only tests through an installed `sshpal-run` helper, with documented task inputs and generated task help

## What It Does

### Project-aware sync

`sshpal` discovers a `.sshpal.toml` file by walking upward from the current working directory until it finds the nearest one.

From there it computes sync paths relative to the project root. If you are in a nested directory and run:

```sh
sshpal push .
```

it syncs the matching subpath from local to remote. `pull` does the inverse.

### Local-only task execution from the remote machine

Run the local daemon with:

```sh
sshpal serve
```

On startup, `sshpal serve` installs or refreshes the remote `sshpal-run` script at `remote_bin_path`, then starts the reverse tunnel and local RPC server.

From the remote machine, run:

```sh
sshpal-run test
```

The helper script posts to the local daemon through the reverse tunnel and streams stdout, stderr, and exit code back to the remote terminal.

## Configuration

The config file name is:

```text
.sshpal.toml
```

Place it at the project root. `sshpal` will walk upward from your current directory until it finds the nearest config file and will use that directory as the project root.

### Required fields

- `ssh_target`
- `remote_root`

### Optional fields

- `local_root`
  - default: the directory containing `.sshpal.toml`
- `rpc_port`
  - default: `45678`
- `remote_bin_path`
  - default: `"~/.local/bin/sshpal-run"`
- `sync_detection_timeout`
  - default: `10s`
- `tasks`
  - default: empty

### Task formats

Tasks support both shorthand and structured forms.

Shorthand examples:

```toml
[tasks]
test = ["cargo", "test"]
check = [["cargo", "fmt", "--check"], ["cargo", "test"]]
dev = "bun run dev"
```

Structured tasks add descriptions and documented variables:

```toml
[tasks.build]
run = "cargo build --package '{#crate}'"
description = "Build one Cargo package"
cwd = "."
timeout = "10m"

[tasks.build.env]
RUST_BACKTRACE = "1"

[tasks.build.vars.crate]
description = "Cargo package name"
```

Task variables use `{#name}` placeholders. Environment variables use `{$NAME}`. Use `{{` and `}}` for literal braces. Structured tasks may also set `cwd`, `timeout`, and `[tasks.<name>.env]`.

String tasks are split into argv with shell-like quoting, but `sshpal` does not execute them through a shell. If you want shell semantics, invoke `sh -c` explicitly.

### Config examples

- Minimal example: [examples/minimal.sshpal.toml](/Users/frk/dev/sshpal/examples/minimal.sshpal.toml)
- Complete example: [examples/complete.sshpal.toml](/Users/frk/dev/sshpal/examples/complete.sshpal.toml)

## Commands

### `sshpal push <path>`

Sync a local path to the corresponding remote path with `rsync --delete`.

Example:

```sh
sshpal push .
sshpal push src
sshpal push src/lib.rs
```

### `sshpal pull <path>`

Sync a remote path back to the corresponding local path.

Example:

```sh
sshpal pull .
sshpal pull src
```

### `sshpal serve`

Installs the remote `sshpal-run` helper, starts the local RPC daemon on `127.0.0.1:<rpc_port>`, and spawns the reverse SSH tunnel used by the remote helper.

If the port is already in use, `sshpal` fails with guidance to:

- shut down existing `sshpal` servers
- or choose another port with `rpc_port` in config

### `sshpal run <task> [name=value ...] [-- <args...>]`

Run a configured task locally using the same task model as the remote helper.

Examples:

```sh
sshpal run build crate=my-crate
sshpal run build crate=my-crate -- --release
```

Values with spaces use normal shell quoting:

```sh
sshpal run build crate="my crate"
```

Arguments before `--` must be `name=value`. Arguments after `--` are forwarded to the final step only.

### `sshpal checkhealth`

Run local and remote setup checks for the current project. This validates config discovery, required local tools, task working directories, RPC port availability, and remote helper prerequisites.

### `sshpal tasks-help`

Print generated task documentation for the current project.

## Example Workflow

### 1. Add config

Create `.sshpal.toml` in your project root:

```toml
ssh_target = "you@remote-host"
remote_root = "/work/project"

[tasks]
test = ["bin/test"]
check = [["cargo", "fmt", "--check"], ["cargo", "test"]]

[tasks.build]
run = "cargo build --package '{#crate}'"
description = "Build one Cargo package"

[tasks.build.vars.crate]
description = "Cargo package name"
```

### 2. Start the local daemon

On the local machine:

```sh
sshpal serve
```

This installs the remote helper to `~/.local/bin/sshpal-run` by default.

### 3. Run tasks from the remote machine

On the remote machine:

```sh
sshpal-run test
sshpal-run build crate=my-crate
sshpal-run tasks-help
sshpal-run checkhealth
```

Normal remote task execution now waits for a synced sentinel file at `.sshpal/sync-token` before running the local task. `sshpal` stays sync-tool agnostic: it does not perform synchronization itself, but it waits up to `sync_detection_timeout` for the token written on the remote machine to appear locally. `tasks-help` and `checkhealth` bypass this sync wait.

Tasks can be defined as a string command, a single command array, or a sequence of command arrays. For sequential tasks, commands run in order and stop on the first non-zero exit code. Client-provided vars use `name=value` before `--`. Any extra args passed after `--` are appended to the final command in the sequence.

### 4. Sync files in either direction

From any directory inside the local project:

```sh
sshpal push .
sshpal pull .
```

## Runtime Requirements

For local development:

- Rust toolchain

For the remote helper:

- `/bin/sh`
- `curl`
- `jq`

## Testing

Run the standard test suite with:

```sh
cargo test
```

There is also an ignored Docker-based integration test that exercises the `sshpal-run` remote helper behavior:

```sh
cargo test --test docker_remote -- --ignored --nocapture
```

That test requires Docker, network access for image/package pulls, and extra time to install packages in-container.

## Coverage

Generate code coverage with:

```sh
./scripts/coverage.sh
```

This uses `cargo-llvm-cov` and writes:

- HTML report: `target/coverage/html/index.html`
- LCOV report: `target/coverage/lcov.info`

If `cargo-llvm-cov` is not installed, the script will fail with the install command:

```sh
cargo install cargo-llvm-cov
```

To include the ignored Docker integration test in coverage, run:

```sh
./scripts/coverage.sh --include-docker
```

That requires Docker and is much slower than the default coverage run.
