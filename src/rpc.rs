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
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::{Config, Task};

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

pub fn remote_helper_script(port: u16) -> String {
    include_str!("sshpal-run.sh").replace("__SSHPAL_RPC_PORT__", &port.to_string())
}

#[derive(Clone)]
struct RpcState {
    tasks: BTreeMap<String, Task>,
}

pub async fn serve(config: Config) -> Result<()> {
    let rpc_port = config.rpc_port;
    let state = RpcState {
        tasks: config.tasks,
    };
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
    eprintln!("sshpal: serve startup finished; listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .with_context(|| format!("RPC server failed on port {rpc_port}"))
}

async fn run_task(
    State(state): State<RpcState>,
    Json(request): Json<RpcRequest>,
) -> Result<Response, RpcResponseError> {
    eprintln!(
        "sshpal: task invoked `{}`{}",
        request.task,
        format_invocation_args(&request.args)
    );
    let task = state.tasks.get(&request.task).cloned().ok_or_else(|| {
        RpcResponseError::new(
            StatusCode::NOT_FOUND,
            format!("unknown task `{}`", request.task),
        )
    })?;

    let body_stream = stream! {
        let mut exit_code = 0;

        for (index, step) in task.steps.iter().enumerate() {
            let argv = augment_step(step.clone(), &request.args, index + 1 == task.steps.len());
            eprintln!(
                "sshpal: starting step {}/{} for `{}`: {}",
                index + 1,
                task.steps.len(),
                request.task,
                format_step(&argv)
            );

            let Some(program) = argv.first().cloned() else {
                let event = serde_json::to_string(&RpcEvent::Stderr {
                    chunk: "task command is empty\n".to_string(),
                }).unwrap();
                yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                exit_code = 1;
                break;
            };
            let args = argv.into_iter().skip(1).collect::<Vec<_>>();

            let mut child = match Command::new(program)
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn() {
                Ok(child) => child,
                Err(err) => {
                    let event = serde_json::to_string(&RpcEvent::Stderr {
                        chunk: format!("{err}\n"),
                    }).unwrap();
                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                    exit_code = 1;
                    break;
                }
            };

            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let event = serde_json::to_string(&RpcEvent::Stderr {
                        chunk: "missing child stdout\n".to_string(),
                    }).unwrap();
                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                    exit_code = 1;
                    break;
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    let event = serde_json::to_string(&RpcEvent::Stderr {
                        chunk: "missing child stderr\n".to_string(),
                    }).unwrap();
                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                    exit_code = 1;
                    break;
                }
            };

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
            exit_code = code;
            if code != 0 {
                break;
            }
        }

        let exit = serde_json::to_string(&RpcEvent::Exit { code: exit_code }).unwrap();
        yield Ok::<_, std::convert::Infallible>(format!("{exit}\n"));
    };

    let mut response = Body::from_stream(body_stream).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    Ok(response)
}

fn format_invocation_args(args: &[String]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        format!(" with args [{}]", args.join(", "))
    }
}

fn augment_step(mut step: Vec<String>, args: &[String], is_final_step: bool) -> Vec<String> {
    if is_final_step {
        step.extend(args.iter().cloned());
    }
    step
}

fn format_step(argv: &[String]) -> String {
    argv.join(" ")
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
            RpcEvent::Stdout {
                chunk: format!("{line}\n"),
            }
        } else {
            RpcEvent::Stderr {
                chunk: format!("{line}\n"),
            }
        };
        let serialized = serde_json::to_string(&event)
            .map(|s| format!("{s}\n"))
            .map_err(|e| anyhow!(e));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use futures_util::StreamExt;
    use reqwest::Client;
    use std::net::TcpListener as StdTcpListener;
    use tokio::time::{Duration, sleep};

    fn config_for(port: u16) -> Config {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "test".to_string(),
            Task {
                steps: vec![vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo out; echo err >&2; exit 7".to_string(),
                ]],
            },
        );
        tasks.insert(
            "slow".to_string(),
            Task {
                steps: vec![vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo first; sleep 0.2; echo second; exit 0".to_string(),
                ]],
            },
        );
        tasks.insert(
            "no_newline".to_string(),
            Task {
                steps: vec![vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf out".to_string(),
                ]],
            },
        );
        tasks.insert(
            "sequence".to_string(),
            Task {
                steps: vec![
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "echo first".to_string(),
                    ],
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "echo second \"$0\"; exit 0".to_string(),
                    ],
                ],
            },
        );
        tasks.insert(
            "sequence_fails".to_string(),
            Task {
                steps: vec![
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "echo before-fail".to_string(),
                    ],
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "echo boom >&2; exit 9".to_string(),
                    ],
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "echo after-fail".to_string(),
                    ],
                ],
            },
        );
        Config {
            ssh_target: "me@example".to_string(),
            local_root: "/tmp/local".into(),
            remote_root: "/tmp/remote".into(),
            rpc_port: port,
            remote_bin_path: "~/.local/bin/sshpal-run".to_string(),
            tasks,
        }
    }

    async fn collect_events(
        config: &Config,
        task: &str,
        args: Vec<String>,
    ) -> Result<Vec<RpcEvent>> {
        let url = format!("http://127.0.0.1:{}/run", config.rpc_port);
        let response = Client::builder()
            .build()?
            .post(url)
            .json(&RpcRequest {
                task: task.to_string(),
                args,
            })
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
        let mut events = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            carry.extend_from_slice(&chunk);
            while let Some(pos) = carry.iter().position(|b| *b == b'\n') {
                let line = carry.drain(..=pos).collect::<Vec<_>>();
                if line.len() <= 1 {
                    continue;
                }
                events.push(serde_json::from_slice(&line[..line.len() - 1])?);
            }
        }
        Ok(events)
    }

    #[tokio::test]
    async fn rpc_serializes() {
        let req = RpcRequest {
            task: "test".to_string(),
            args: vec!["a".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let roundtrip: RpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, req);
    }

    #[tokio::test]
    async fn run_endpoint_streams_stdout_stderr_and_exit() {
        let port = 49001;
        let cfg = config_for(port);
        let server_cfg = cfg.clone();
        let handle = tokio::spawn(async move { serve(server_cfg).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(&cfg, "test", Vec::new()).await.unwrap();
        handle.abort();
        assert_eq!(
            events,
            vec![
                RpcEvent::Stdout {
                    chunk: "out\n".to_string()
                },
                RpcEvent::Stderr {
                    chunk: "err\n".to_string()
                },
                RpcEvent::Exit { code: 7 },
            ]
        );
    }

    #[tokio::test]
    async fn unknown_task_is_rejected() {
        let port = 49002;
        let cfg = config_for(port);
        let server_cfg = cfg.clone();
        let handle = tokio::spawn(async move { serve(server_cfg).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let err = collect_events(&cfg, "missing", Vec::new())
            .await
            .unwrap_err();
        handle.abort();
        assert!(err.to_string().contains("RPC request failed"));
    }

    #[tokio::test]
    async fn run_endpoint_executes_task_steps_sequentially() {
        let port = 49003;
        let cfg = config_for(port);
        let server_cfg = cfg.clone();
        let handle = tokio::spawn(async move { serve(server_cfg).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(&cfg, "sequence", vec!["arg".to_string()])
            .await
            .unwrap();
        handle.abort();
        assert_eq!(
            events,
            vec![
                RpcEvent::Stdout {
                    chunk: "first\n".to_string()
                },
                RpcEvent::Stdout {
                    chunk: "second arg\n".to_string()
                },
                RpcEvent::Exit { code: 0 },
            ]
        );
    }

    #[tokio::test]
    async fn run_endpoint_stops_on_first_failing_step() {
        let port = 49004;
        let cfg = config_for(port);
        let server_cfg = cfg.clone();
        let handle = tokio::spawn(async move { serve(server_cfg).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(&cfg, "sequence_fails", Vec::new())
            .await
            .unwrap();
        handle.abort();
        assert_eq!(
            events,
            vec![
                RpcEvent::Stdout {
                    chunk: "before-fail\n".to_string()
                },
                RpcEvent::Stderr {
                    chunk: "boom\n".to_string()
                },
                RpcEvent::Exit { code: 9 },
            ]
        );
    }

    #[test]
    fn remote_helper_script_embeds_port_and_command_name() {
        let script = remote_helper_script(45678);
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("usage: sshpal-run <task> [args...]"));
        assert!(script.contains("http://127.0.0.1:45678/run"));
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
