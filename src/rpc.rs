use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use async_stream::stream;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub task: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcEvent {
    Stdout { chunk: String },
    Stderr { chunk: String },
    Exit { code: i32 },
}

#[derive(Clone)]
struct RpcState {
    tasks: BTreeMap<String, Vec<String>>,
}

pub async fn serve(config: Config) -> Result<()> {
    let state = RpcState { tasks: config.tasks };
    let app = Router::new()
        .route("/run", post(run_task))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], config.rpc_port));
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            bail!(
                "failed to bind RPC server on {addr}: port already in use; shut down existing sshpal servers or use the rpc_port config option"
            );
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to bind RPC server on {}", addr));
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("RPC server failed")
}

async fn run_task(
    State(state): State<RpcState>,
    Json(request): Json<RpcRequest>,
) -> Result<Response, RpcResponseError> {
    let base = state
        .tasks
        .get(&request.task)
        .cloned()
        .ok_or_else(|| RpcResponseError::new(StatusCode::NOT_FOUND, format!("unknown task `{}`", request.task)))?;
    let mut argv = base;
    argv.extend(request.args);
    let program = argv
        .first()
        .cloned()
        .ok_or_else(|| RpcResponseError::new(StatusCode::BAD_REQUEST, "task command is empty".to_string()))?;
    let args = argv.into_iter().skip(1).collect::<Vec<_>>();

    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| RpcResponseError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let stdout = child.stdout.take().ok_or_else(|| {
        RpcResponseError::new(StatusCode::INTERNAL_SERVER_ERROR, "missing child stdout".to_string())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        RpcResponseError::new(StatusCode::INTERNAL_SERVER_ERROR, "missing child stderr".to_string())
    })?;

    let body_stream = stream! {
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<String, anyhow::Error>>();
        tokio::spawn(pump_reader(stdout, tx.clone(), true));
        tokio::spawn(pump_reader(stderr, tx, false));

        while let Some(item) = rx.recv().await {
            match item {
                Ok(line) => yield Ok::<_, std::convert::Infallible>(line),
                Err(err) => {
                    let event = serde_json::to_string(&RpcEvent::Stderr { chunk: format!("stream error: {err}\n") }).unwrap();
                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                }
            }
        }

        let code = match child.wait().await {
            Ok(status) => status.code().unwrap_or(1),
            Err(err) => {
                let event = serde_json::to_string(&RpcEvent::Stderr { chunk: format!("wait error: {err}\n") }).unwrap();
                yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                1
            }
        };
        let exit = serde_json::to_string(&RpcEvent::Exit { code }).unwrap();
        yield Ok::<_, std::convert::Infallible>(format!("{exit}\n"));
    };

    let mut response = Body::from_stream(body_stream).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    Ok(response)
}

async fn pump_reader<R>(
    reader: R,
    tx: mpsc::UnboundedSender<Result<String, anyhow::Error>>,
    stdout: bool,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let event = if stdout {
            RpcEvent::Stdout { chunk: format!("{line}\n") }
        } else {
            RpcEvent::Stderr { chunk: format!("{line}\n") }
        };
        let serialized = serde_json::to_string(&event).map(|s| format!("{s}\n")).map_err(|e| anyhow!(e));
        let _ = tx.send(serialized);
    }
}

#[derive(Debug)]
struct RpcResponseError {
    status: StatusCode,
    message: String,
}

impl RpcResponseError {
    fn new(status: StatusCode, message: String) -> Self {
        Self { status, message }
    }
}

impl IntoResponse for RpcResponseError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

pub async fn other_run(config: &Config, task: String, args: Vec<String>) -> Result<i32> {
    let url = format!("http://127.0.0.1:{}/run", config.rpc_port);
    let client = Client::builder().build()?;
    let response = client
        .post(url)
        .json(&RpcRequest { task, args })
        .send()
        .await
        .context("failed to contact local RPC daemon")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("RPC request failed: {status} {text}");
    }

    let mut stream = response.bytes_stream();
    let mut carry = Vec::<u8>::new();
    let mut exit_code = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        carry.extend_from_slice(&chunk);
        while let Some(pos) = carry.iter().position(|b| *b == b'\n') {
            let line = carry.drain(..=pos).collect::<Vec<_>>();
            if line.len() <= 1 {
                continue;
            }
            let event: RpcEvent = serde_json::from_slice(&line[..line.len() - 1])?;
            match event {
                RpcEvent::Stdout { chunk } => {
                    print!("{chunk}");
                }
                RpcEvent::Stderr { chunk } => {
                    eprint!("{chunk}");
                }
                RpcEvent::Exit { code } => {
                    exit_code = Some(code);
                }
            }
        }
    }
    exit_code.ok_or_else(|| anyhow!("RPC stream ended without exit event"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RemoteArch};
    use std::net::TcpListener as StdTcpListener;
    use tokio::time::{Duration, sleep};

    fn config_for(port: u16) -> Config {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "test".to_string(),
            vec!["sh".to_string(), "-c".to_string(), "echo out; echo err >&2; exit 7".to_string()],
        );
        tasks.insert(
            "slow".to_string(),
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo first; sleep 0.2; echo second; exit 0".to_string(),
            ],
        );
        Config {
            ssh_target: "me@example".to_string(),
            local_root: "/tmp/local".into(),
            remote_root: "/tmp/remote".into(),
            remote_arch: RemoteArch::X86_64,
            rpc_port: port,
            remote_bin_path: "~/.local/bin/sshpal".to_string(),
            tasks,
        }
    }

    #[tokio::test]
    async fn rpc_serializes() {
        let req = RpcRequest { task: "test".to_string(), args: vec!["a".to_string()] };
        let json = serde_json::to_string(&req).unwrap();
        let roundtrip: RpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, req);
    }

    #[tokio::test]
    async fn other_run_returns_exit_code() {
        let port = 49001;
        let cfg = config_for(port);
        let server_cfg = cfg.clone();
        let handle = tokio::spawn(async move { serve(server_cfg).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let code = other_run(&cfg, "test".to_string(), Vec::new()).await.unwrap();
        handle.abort();
        assert_eq!(code, 7);
    }

    #[tokio::test]
    async fn unknown_task_is_rejected() {
        let port = 49002;
        let cfg = config_for(port);
        let server_cfg = cfg.clone();
        let handle = tokio::spawn(async move { serve(server_cfg).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let err = other_run(&cfg, "missing".to_string(), Vec::new()).await.unwrap_err();
        handle.abort();
        assert!(err.to_string().contains("RPC request failed"));
    }

    #[tokio::test]
    async fn serve_reports_actionable_error_when_port_is_taken() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = serve(config_for(port)).await.unwrap_err().to_string();
        assert!(err.contains("port already in use"));
        assert!(err.contains("shut down existing sshpal servers"));
        assert!(err.contains("rpc_port config option"));
    }
}
