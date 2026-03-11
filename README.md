# sshpal

`sshpal` is a Rust CLI for working with a remote Linux machine over SSH when that machine cannot fetch code or build tools from the internet.

It provides three main features:

- project-aware `push` / `pull` commands built on `rsync`
- a local RPC server so the remote machine can trigger local-only tasks such as macOS-only tests
- remote binary installation, so the Linux box can run `sshpal` without needing Cargo, Git, or package downloads

## What It Does

### Project-aware sync

`sshpal` discovers a `.sshpal.toml` file by walking upward from the current working directory until it finds the nearest one.

From there it computes sync paths relative to the project root. If you are in a nested directory and run:

```sh
sshpal push .
```

it syncs the matching subpath from local to remote. `pull` does the inverse.

### Local-only task execution from the remote machine

You can run a local daemon with:

```sh
sshpal serve
```

Then, from the remote machine, run:

```sh
sshpal other-run test
```

The remote `sshpal` client sends a request to the local daemon and streams the task's stdout, stderr, and exit code back to the remote terminal.

### Remote installation

`sshpal install-remote` builds a Linux binary locally and copies it to the remote machine so the remote host can use `sshpal` without installing Rust tooling.

## Configuration

The config file name is:

```text
.sshpal.toml
```

Place it at the project root. `sshpal` will walk upward from your current directory until it finds the nearest config file and will use that directory as the project root.

### Required fields

- `ssh_target`
- `remote_root`
- `remote_arch`

### Optional fields

- `local_root`
  - default: the directory containing `.sshpal.toml`
- `rpc_port`
  - default: `45678`
- `remote_bin_path`
  - default: `"~/.local/bin/sshpal"`
- `tasks`
  - default: empty

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

Starts the local RPC daemon on `127.0.0.1:<rpc_port>` and spawns the reverse SSH tunnel used by the remote client.

If the port is already in use, `sshpal` fails with guidance to:

- shut down existing `sshpal` servers
- or choose another port with `rpc_port` in config

### `sshpal other-run <task> [args...]`

Runs a configured local task through the RPC daemon and prints the task output on the remote terminal.

Only named tasks from config are allowed.

Example:

```sh
sshpal other-run test
sshpal other-run lint path/to/file
```

### `sshpal install-remote`

Builds a Linux binary locally and copies it to the remote machine.

Optional override:

```sh
sshpal install-remote --remote-arch aarch64
```

## Example Workflow

### 1. Add config

Create `.sshpal.toml` in your project root:

```toml
ssh_target = "you@remote-host"
remote_root = "/work/project"
remote_arch = "x86_64"

[tasks]
test = ["bin/test"]
```

### 2. Install the remote binary

From the local machine:

```sh
sshpal install-remote
```

This copies the Linux binary to the remote machine, defaulting to `~/.local/bin/sshpal`.

### 3. Start the local daemon

On the local machine:

```sh
sshpal serve
```

### 4. Run tasks from the remote machine

On the remote machine:

```sh
sshpal other-run test
```

### 5. Sync files in either direction

From any directory inside the local project:

```sh
sshpal push .
sshpal pull .
```

## Build Requirements

For normal local development:

- Rust toolchain

For `install-remote`:

- a local Zig toolchain
- `cargo-zigbuild`

`sshpal install-remote` expects to build a static Linux binary locally. The remote machine does not need Rust, Cargo, Git, or internet access.

## Testing

Run the standard test suite with:

```sh
cargo test
```

There is also an ignored Docker-based integration test that exercises the Linux remote client behavior:

```sh
cargo test --test docker_remote -- --ignored --nocapture
```

That test requires Docker, network access for image/package pulls, and extra time to build the Linux binary in-container.

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
