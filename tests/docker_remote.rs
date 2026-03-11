use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

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
        stdout.contains("from stdout\nsecond stdout line\nPROMPT\n"),
        "stdout from stub daemon did not preserve newlines: {stdout}"
    );
    assert!(
        stderr.contains("from stderr\nsecond stderr line\n"),
        "stderr from stub daemon did not preserve newlines: {stderr}"
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
            {{'type': 'stdout', 'chunk': 'second stdout line\n'}},
            {{'type': 'stderr', 'chunk': 'from stderr\n'}},
            {{'type': 'stderr', 'chunk': 'second stderr line\n'}},
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

fn run_remote_container(project_dir: &Path) -> Result<std::process::Output> {
    let remote_script = "set -euo pipefail; \
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
         set +e; \
         sshpal-run test; \
         status=$?; \
         set -e; \
         printf 'PROMPT\n'; \
         if [ $status -ne 0 ]; then \
             echo '--- rpc stub log ---' >&2; \
             cat /tmp/rpc_stub.log >&2 || true; \
             exit $status; \
         fi";

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
