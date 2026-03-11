use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[test]
#[ignore = "requires Docker, image pulls, and network access"]
fn ubuntu_container_remote_client_replays_output_and_exit_code() -> Result<()> {
    ensure_docker()?;
    build_linux_binary_in_docker()?;

    let port = 49091;
    let remote_project = prepare_remote_project_dir(port)?;
    write_remote_config(&remote_project, port)?;

    let output = run_remote_container(&remote_project, port)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(7),
        "unexpected remote exit status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("from stdout"),
        "stdout from stub daemon was not relayed through remote call: {stdout}"
    );
    assert!(
        stderr.contains("from stderr"),
        "stderr from stub daemon was not relayed through remote call: {stderr}"
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

fn build_linux_binary_in_docker() -> Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/work", workspace.display()),
            "-w",
            "/work",
            "-e",
            "CARGO_TARGET_DIR=/work/target/docker-linux",
            "rust:1.88-bookworm",
            "cargo",
            "build",
            "--release",
            "--bin",
            "sshpal",
        ])
        .status()
        .context("failed to build Linux binary in Docker")?;
    if !status.success() {
        bail!("dockerized Linux build failed with status {:?}", status.code());
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
remote_arch = "x86_64"
rpc_port = {port}

[tasks]
test = ["placeholder"]
"#
    );
    fs::write(dir.join(".sshpal.toml"), content).context("failed to write remote test config")?;
    fs::write(dir.join("rpc_stub.py"), rpc_stub_script(port)).context("failed to write rpc stub")?;
    Ok(())
}

fn rpc_stub_script(port: u16) -> String {
    format!(
        r#"import json
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = {port}

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != '/run':
            self.send_response(404)
            self.end_headers()
            return
        length = int(self.headers.get('Content-Length', '0'))
        body = self.rfile.read(length)
        request = json.loads(body.decode('utf-8'))
        if request.get('task') != 'test':
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b'unknown task')
            return
        self.send_response(200)
        self.send_header('Content-Type', 'application/x-ndjson')
        self.end_headers()
        events = [
            {{'type': 'stdout', 'chunk': 'from stdout\n'}},
            {{'type': 'stderr', 'chunk': 'from stderr\n'}},
            {{'type': 'exit', 'code': 7}},
        ]
        for event in events:
            self.wfile.write((json.dumps(event) + '\n').encode('utf-8'))
            self.wfile.flush()

    def log_message(self, format, *args):
        return

HTTPServer(('127.0.0.1', PORT), Handler).serve_forever()
"#
    )
}

fn run_remote_container(project_dir: &Path, _port: u16) -> Result<std::process::Output> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let remote_script = format!(
        "set -euo pipefail; \
         apt-get update >/dev/null; \
         apt-get install -y python3 >/dev/null; \
         cp /work/target/docker-linux/release/sshpal /usr/local/bin/sshpal; \
         chmod +x /usr/local/bin/sshpal; \
         python3 /project/rpc_stub.py >/tmp/rpc_stub.log 2>&1 & \
         stub_pid=$!; \
         trap 'kill $stub_pid 2>/dev/null || true' EXIT; \
         sleep 1; \
         if ! kill -0 $stub_pid 2>/dev/null; then \
             cat /tmp/rpc_stub.log >&2 || true; \
             exit 1; \
         fi; \
         cd /project; \
         sshpal other-run test; \
         status=$?; \
         if [ $status -ne 0 ]; then \
             echo '--- rpc stub log ---' >&2; \
             cat /tmp/rpc_stub.log >&2 || true; \
             exit $status; \
         fi"
    );

    Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/work", workspace.display()),
            "-v",
            &format!("{}:/project", project_dir.display()),
            "-w",
            "/project",
            "ubuntu:24.04",
            "bash",
            "-lc",
            &remote_script,
        ])
        .output()
        .context("failed to run Ubuntu remote container")
}
