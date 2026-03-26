use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

#[test]
#[ignore = "requires Docker, image pulls, and network access"]
fn ubuntu_container_remote_client_replays_output_and_exit_code() -> Result<()> {
    ensure_docker()?;

    let port = 49091;
    let remote_project = prepare_remote_project_dir(port)?;
    write_remote_config(&remote_project, port)?;

    let output = run_remote_container(&remote_project)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(7),
        "unexpected remote exit status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(
            "usage: sshpal-run test [-- <args...>]\nfrom stdout\nsecond stdout line\nPROMPT\n"
        ),
        "stdout from stub daemon did not preserve newlines: {stdout}"
    );
    assert!(
        stderr.contains("from stderr\nsecond stderr line\n"),
        "stderr from stub daemon did not preserve newlines: {stderr}"
    );
    Ok(())
}

#[test]
#[ignore = "requires Docker, image pulls, and network access"]
fn ubuntu_container_remote_client_handles_checkhealth_and_no_newline_output() -> Result<()> {
    ensure_docker()?;

    let port = 49092;
    let remote_project = prepare_remote_project_dir(port)?;
    write_remote_config(&remote_project, port)?;

    let output = run_remote_container_with_script(
        &remote_project,
        "set -euo pipefail; \
         apt-get update >/dev/null; \
         apt-get install -y curl jq python3 >/dev/null; \
         cp /project/sshpal-run /usr/local/bin/sshpal-run; \
         chmod +x /usr/local/bin/sshpal-run; \
         python3 /project/rpc_stub.py >/tmp/rpc_stub.log 2>&1 & \
         stub_pid=$!; \
         trap 'kill $stub_pid 2>/dev/null || true' EXIT; \
         sleep 1; \
         if ! kill -0 $stub_pid 2>/dev/null; then \
             cat /tmp/rpc_stub.log >&2 || true; \
             exit 1; \
         fi; \
         cd /project; \
         sshpal-run checkhealth; \
         sshpal-run no_newline; \
         printf 'AFTER\n';",
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "unexpected remote exit status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("config: ok\n"),
        "missing checkhealth output: {stdout}"
    );
    assert!(
        stdout.contains("no-newlineAFTER\n"),
        "remote helper did not preserve output without trailing newline: {stdout}"
    );
    Ok(())
}

#[test]
#[ignore = "requires Docker, image pulls, and network access"]
fn ubuntu_container_remote_client_reports_sync_timeout_failure() -> Result<()> {
    ensure_docker()?;

    let port = 49093;
    let remote_project = prepare_remote_project_dir(port)?;
    write_remote_config(&remote_project, port)?;

    let output = run_remote_container_with_script(
        &remote_project,
        "set -euo pipefail; \
         apt-get update >/dev/null; \
         apt-get install -y curl jq python3 >/dev/null; \
         cp /project/sshpal-run /usr/local/bin/sshpal-run; \
         chmod +x /usr/local/bin/sshpal-run; \
         python3 /project/rpc_stub.py >/tmp/rpc_stub.log 2>&1 & \
         stub_pid=$!; \
         trap 'kill $stub_pid 2>/dev/null || true' EXIT; \
         sleep 1; \
         cd /project; \
         set +e; \
         sshpal-run sync_timeout; \
         status=$?; \
         set -e; \
         printf 'status=%s\n' \"$status\"; \
         exit 0",
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("status=124\n"),
        "unexpected helper exit status: {stdout}"
    );
    assert!(
        stderr.contains("timed out waiting 10s for synced sentinel"),
        "missing sync-timeout message: {stderr}"
    );
    Ok(())
}

#[test]
#[ignore = "requires Docker, image pulls, and network access"]
fn ubuntu_container_remote_client_runs_help_and_checkhealth_without_jq() -> Result<()> {
    ensure_docker()?;

    let port = 49094;
    let remote_project = prepare_remote_project_dir(port)?;
    write_remote_config(&remote_project, port)?;

    let output = run_remote_container_with_script(
        &remote_project,
        "set -euo pipefail; \
         apt-get update >/dev/null; \
         apt-get install -y curl python3 >/dev/null; \
         cp /project/sshpal-run /usr/local/bin/sshpal-run; \
         chmod +x /usr/local/bin/sshpal-run; \
         python3 /project/rpc_stub.py >/tmp/rpc_stub.log 2>&1 & \
         stub_pid=$!; \
         trap 'kill $stub_pid 2>/dev/null || true' EXIT; \
         sleep 1; \
         cd /project; \
         sshpal-run tasks-help; \
         sshpal-run checkhealth",
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "unexpected remote exit status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("usage: sshpal-run test [-- <args...>]\n"));
    assert!(stdout.contains("config: ok\n"));
    assert!(
        !stderr.contains("requires jq"),
        "pseudo-tasks should not require jq: {stderr}"
    );
    Ok(())
}

#[test]
#[ignore = "requires Docker, image pulls, and network access"]
fn ubuntu_container_remote_client_forwards_args_to_rpc() -> Result<()> {
    ensure_docker()?;

    let port = 49095;
    let remote_project = prepare_remote_project_dir(port)?;
    write_remote_config(&remote_project, port)?;

    let output = run_remote_container_with_script(
        &remote_project,
        "set -euo pipefail; \
         apt-get update >/dev/null; \
         apt-get install -y curl jq python3 >/dev/null; \
         cp /project/sshpal-run /usr/local/bin/sshpal-run; \
         chmod +x /usr/local/bin/sshpal-run; \
         python3 /project/rpc_stub.py >/tmp/rpc_stub.log 2>&1 & \
         stub_pid=$!; \
         trap 'kill $stub_pid 2>/dev/null || true' EXIT; \
         sleep 1; \
         cd /project; \
         sshpal-run arg_echo -- --watch=false sample arg",
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "unexpected remote exit status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("--watch=false|sample|arg\n"),
        "forwarded args did not reach RPC stub: {stdout}"
    );
    Ok(())
}

fn ensure_docker() -> Result<()> {
    let status = Command::new("docker")
        .arg("--version")
        .status()
        .context("failed to invoke docker")?;
    if !status.success() {
        bail!("docker is not available");
    }
    Ok(())
}

fn prepare_remote_project_dir(port: u16) -> Result<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("docker-remote-test")
        .join(port.to_string());
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn write_remote_config(dir: &Path, port: u16) -> Result<()> {
    let content = format!(
        r#"
ssh_target = "container@example"
remote_root = "/workspace/project"
rpc_port = {port}

[tasks]
test = ["placeholder"]
no_newline = ["placeholder"]
sync_timeout = ["placeholder"]
arg_echo = ["placeholder"]
"#
    );
    fs::write(dir.join(".sshpal.toml"), content).context("failed to write remote test config")?;
    fs::write(dir.join("rpc_stub.py"), rpc_stub_script(port))
        .context("failed to write rpc stub")?;
    fs::write(
        dir.join("sshpal-run"),
        sshpal::rpc::remote_helper_script(port),
    )
    .context("failed to write sshpal-run helper")?;
    Ok(())
}

fn rpc_stub_script(port: u16) -> String {
    format!(
        r#"import base64
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = {port}

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/tasks-help':
            self.send_response(200)
            self.send_header('Content-Type', 'text/plain; charset=utf-8')
            self.end_headers()
            self.wfile.write(b'usage: sshpal-run test [-- <args...>]\n')
            return
        if self.path == '/checkhealth':
            self.send_response(200)
            self.send_header('Content-Type', 'text/plain; charset=utf-8')
            self.end_headers()
            self.wfile.write(b'config: ok\n')
            return
        if self.path != '/tasks-help':
            self.send_response(404)
            self.end_headers()
            return

    def do_POST(self):
        if self.path != '/run':
            self.send_response(404)
            self.end_headers()
            return
        length = int(self.headers.get('Content-Length', '0'))
        body = self.rfile.read(length)
        request = json.loads(body.decode('utf-8'))
        task = request.get('task')
        if task not in ('test', 'no_newline', 'sync_timeout', 'arg_echo'):
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b'unknown task')
            return
        if not request.get('sync_token'):
            self.send_response(400)
            self.end_headers()
            self.wfile.write(b'missing sync token')
            return
        self.send_response(200)
        self.send_header('Content-Type', 'application/x-ndjson')
        self.end_headers()
        if task == 'sync_timeout':
            events = [
                {{'type': 'stderr', 'chunk_b64': '{stderr_sync_timeout}'}},
                {{'type': 'exit', 'code': 124}},
            ]
        elif task == 'test':
            events = [
                {{'type': 'stdout', 'chunk_b64': '{stdout1}'}},
                {{'type': 'stdout', 'chunk_b64': '{stdout2}'}},
                {{'type': 'stderr', 'chunk_b64': '{stderr1}'}},
                {{'type': 'stderr', 'chunk_b64': '{stderr2}'}},
                {{'type': 'exit', 'code': 7}},
            ]
        else:
            if task == 'no_newline':
                events = [
                    {{'type': 'stdout', 'chunk_b64': '{stdout_no_newline}'}},
                    {{'type': 'exit', 'code': 0}},
                ]
            else:
                forwarded = '|'.join(request.get('args', [])) + '\n'
                events = [
                    {{'type': 'stdout', 'chunk_b64': base64.b64encode(forwarded.encode('utf-8')).decode('ascii')}},
                    {{'type': 'exit', 'code': 0}},
                ]
        for event in events:
            self.wfile.write((json.dumps(event) + '\n').encode('utf-8'))
            self.wfile.flush()

    def log_message(self, format, *args):
        return

HTTPServer(('127.0.0.1', PORT), Handler).serve_forever()
"#,
        stdout1 = BASE64.encode(b"from stdout\n"),
        stdout2 = BASE64.encode(b"second stdout line\n"),
        stderr1 = BASE64.encode(b"from stderr\n"),
        stderr2 = BASE64.encode(b"second stderr line\n"),
        stderr_sync_timeout = BASE64.encode(b"timed out waiting 10s for synced sentinel `/workspace/project/.sshpal/sync-token`; remote changes likely have not propagated to the local machine yet\n"),
        stdout_no_newline = BASE64.encode(b"no-newline")
    )
}

fn run_remote_container(project_dir: &Path) -> Result<std::process::Output> {
    run_remote_container_with_script(
        project_dir,
        "set -euo pipefail; \
         apt-get update >/dev/null; \
         apt-get install -y curl jq python3 >/dev/null; \
         cp /project/sshpal-run /usr/local/bin/sshpal-run; \
         chmod +x /usr/local/bin/sshpal-run; \
         python3 /project/rpc_stub.py >/tmp/rpc_stub.log 2>&1 & \
         stub_pid=$!; \
         trap 'kill $stub_pid 2>/dev/null || true' EXIT; \
         sleep 1; \
         if ! kill -0 $stub_pid 2>/dev/null; then \
              cat /tmp/rpc_stub.log >&2 || true; \
              exit 1; \
          fi; \
          cd /project; \
          sshpal-run tasks-help; \
          set +e; \
          sshpal-run test; \
          status=$?; \
         set -e; \
         printf 'PROMPT\n'; \
          if [ $status -ne 0 ]; then \
              echo '--- rpc stub log ---' >&2; \
              cat /tmp/rpc_stub.log >&2 || true; \
              exit $status; \
          fi",
    )
}

fn run_remote_container_with_script(
    project_dir: &Path,
    remote_script: &str,
) -> Result<std::process::Output> {
    Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/project", project_dir.display()),
            "-w",
            "/project",
            "ubuntu:24.04",
            "bash",
            "-lc",
            remote_script,
        ])
        .output()
        .context("failed to run Ubuntu remote container")
}
