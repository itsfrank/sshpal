use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use async_stream::stream;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};

use crate::config::{LoadedConfig, Task};
use crate::health;
use crate::tasks;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub task: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub sync_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcEvent {
    Stdout { chunk_b64: String },
    Stderr { chunk_b64: String },
    Exit { code: i32 },
}

pub fn remote_helper_script(port: u16) -> String {
    include_str!("sshpal-run.sh").replace("__SSHPAL_RPC_PORT__", &port.to_string())
}

#[derive(Clone)]
struct RpcState {
    loaded: LoadedConfig,
    tasks: BTreeMap<String, Task>,
    local_root: PathBuf,
    sync_detection_timeout: std::time::Duration,
}

pub async fn serve(loaded: LoadedConfig) -> Result<()> {
    let rpc_port = loaded.config.rpc_port;
    let state = RpcState {
        tasks: loaded.config.tasks.clone(),
        local_root: loaded.config.local_root.clone(),
        sync_detection_timeout: loaded.config.sync_detection_timeout,
        loaded,
    };
    let app = Router::new()
        .route("/run", post(run_task))
        .route("/tasks-help", get(tasks_help))
        .route("/checkhealth", get(checkhealth))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], rpc_port));
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
    let prepared = tasks::prepare_task(
        &request.task,
        &task,
        &state.local_root,
        &request.vars,
        &request.args,
    )
        .map_err(|err| RpcResponseError::new(StatusCode::BAD_REQUEST, err.to_string()))?;

    if request.task != tasks::TASKS_HELP_NAME && request.task != tasks::CHECKHEALTH_NAME {
        let Some(sync_token) = request.sync_token.as_deref() else {
            return Err(RpcResponseError::new(
                StatusCode::BAD_REQUEST,
                "missing sync token for remote task execution".to_string(),
            ));
        };
        wait_for_sync(&state, sync_token)
            .await
            .map_err(|err| RpcResponseError::new(StatusCode::REQUEST_TIMEOUT, err))?;
    }

    let body_stream = stream! {
        let mut exit_code = 0;

        for (index, argv) in prepared.steps.iter().enumerate() {
            eprintln!(
                "sshpal: starting step {}/{} for `{}`: {}",
                index + 1,
                prepared.steps.len(),
                request.task,
                format_step(argv)
            );

            let Some(program) = argv.first().cloned() else {
                let event = serialize_chunk_event(false, b"task command is empty\n").unwrap();
                yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                exit_code = 1;
                break;
            };
            let args = argv.iter().skip(1).cloned().collect::<Vec<_>>();

            let mut child = match Command::new(program)
                .args(args)
                .current_dir(&prepared.cwd)
                .envs(&prepared.env)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn() {
                Ok(child) => child,
                Err(err) => {
                    let event = serialize_chunk_event(false, format!("{err}\n").as_bytes()).unwrap();
                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                    exit_code = 1;
                    break;
                }
            };

            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let event = serialize_chunk_event(false, b"missing child stdout\n").unwrap();
                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                    exit_code = 1;
                    break;
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    let event = serialize_chunk_event(false, b"missing child stderr\n").unwrap();
                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                    exit_code = 1;
                    break;
                }
            };

            let (tx, mut rx) = mpsc::unbounded_channel::<Result<String, anyhow::Error>>();
            tokio::spawn(pump_reader(stdout, tx.clone(), true));
            tokio::spawn(pump_reader(stderr, tx, false));

            let mut timeout = prepared.timeout.map(|value| Box::pin(time::sleep(value)));
            let code = loop {
                if let Some(timeout_future) = timeout.as_mut() {
                    tokio::select! {
                        item = rx.recv() => {
                            match item {
                                Some(Ok(line)) => yield Ok::<_, std::convert::Infallible>(line),
                                Some(Err(err)) => {
                                    let event = serialize_chunk_event(false, format!("stream error: {err}\n").as_bytes()).unwrap();
                                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                                }
                                None => {
                                    match child.wait().await {
                                        Ok(status) => break status.code().unwrap_or(1),
                                        Err(err) => {
                                            let event = serialize_chunk_event(false, format!("wait error: {err}\n").as_bytes()).unwrap();
                                            yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                                            break 1;
                                        }
                                    }
                                }
                            }
                        }
                        status = child.wait() => {
                            match status {
                                Ok(status) => break status.code().unwrap_or(1),
                                Err(err) => {
                                    let event = serialize_chunk_event(false, format!("wait error: {err}\n").as_bytes()).unwrap();
                                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                                    break 1;
                                }
                            }
                        }
                        _ = timeout_future.as_mut() => {
                            let _ = child.kill().await;
                            let timed_out_after = prepared.timeout.unwrap();
                            let event = serialize_chunk_event(false, format!("task `{}` timed out after {}\n", request.task, humantime::format_duration(timed_out_after)).as_bytes()).unwrap();
                            yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                            let _ = child.wait().await;
                            break 124;
                        }
                    }
                } else {
                    tokio::select! {
                        item = rx.recv() => {
                            match item {
                                Some(Ok(line)) => yield Ok::<_, std::convert::Infallible>(line),
                                Some(Err(err)) => {
                                    let event = serialize_chunk_event(false, format!("stream error: {err}\n").as_bytes()).unwrap();
                                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                                }
                                None => {
                                    match child.wait().await {
                                        Ok(status) => break status.code().unwrap_or(1),
                                        Err(err) => {
                                            let event = serialize_chunk_event(false, format!("wait error: {err}\n").as_bytes()).unwrap();
                                            yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                                            break 1;
                                        }
                                    }
                                }
                            }
                        }
                        status = child.wait() => {
                            match status {
                                Ok(status) => break status.code().unwrap_or(1),
                                Err(err) => {
                                    let event = serialize_chunk_event(false, format!("wait error: {err}\n").as_bytes()).unwrap();
                                    yield Ok::<_, std::convert::Infallible>(format!("{event}\n"));
                                    break 1;
                                }
                            }
                        }
                    }
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

async fn tasks_help(State(state): State<RpcState>) -> Result<String, RpcResponseError> {
    tasks::task_help("sshpal-run", &state.tasks)
        .map_err(|err| RpcResponseError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

async fn checkhealth(State(state): State<RpcState>) -> Result<String, RpcResponseError> {
    health::checkhealth(&state.loaded)
        .map(|report| report.text)
        .map_err(|err| RpcResponseError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

fn format_invocation_args(args: &[String]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        format!(" with args [{}]", args.join(", "))
    }
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
    let mut reader = reader;
    let mut buffer = vec![0_u8; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                let serialized = serialize_chunk_event(stdout, &buffer[..count])
                    .map(|s| format!("{s}\n"))
                    .map_err(|e| anyhow!(e));
                let _ = tx.send(serialized);
            }
            Err(err) => {
                let _ = tx.send(Err(anyhow!(err)));
                break;
            }
        }
    }
}

fn serialize_chunk_event(stdout: bool, chunk: &[u8]) -> serde_json::Result<String> {
    let chunk_b64 = BASE64.encode(chunk);
    let event = if stdout {
        RpcEvent::Stdout { chunk_b64 }
    } else {
        RpcEvent::Stderr { chunk_b64 }
    };
    serde_json::to_string(&event)
}

async fn wait_for_sync(state: &RpcState, sync_token: &str) -> std::result::Result<(), String> {
    let sentinel = health::sentinel_path(&state.local_root);
    let deadline = Instant::now() + state.sync_detection_timeout;
    loop {
        match tokio::fs::read_to_string(&sentinel).await {
            Ok(contents) if contents.trim_end() == sync_token => return Ok(()),
            Ok(_) | Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting {} for synced sentinel `{}`; remote changes likely have not propagated to the local machine yet",
                humantime::format_duration(state.sync_detection_timeout),
                sentinel.display()
            ));
        }
        time::sleep(std::time::Duration::from_millis(100)).await;
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
    use crate::config::{CONFIG_FILE_NAME, Config, DEFAULT_SYNC_DETECTION_TIMEOUT, LoadedConfig};
    use futures_util::StreamExt;
    use reqwest::Client;
    use std::fs;
    use std::net::TcpListener as StdTcpListener;
    use tokio::time::{Duration, sleep};

    fn config_for(port: u16) -> Config {
        let local_root = std::env::temp_dir().join(format!("sshpal-rpc-{port}"));
        fs::create_dir_all(local_root.join(".sshpal")).unwrap();
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "test".to_string(),
            Task {
                run: crate::config::TaskRun::Command(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo out; echo err >&2; exit 7".to_string(),
                ]),
                description: None,
                cwd: None,
                env: BTreeMap::new(),
                timeout: None,
                vars: BTreeMap::new(),
            },
        );
        tasks.insert(
            "slow".to_string(),
            Task {
                run: crate::config::TaskRun::Command(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo first; sleep 0.2; echo second; exit 0".to_string(),
                ]),
                description: None,
                cwd: None,
                env: BTreeMap::new(),
                timeout: None,
                vars: BTreeMap::new(),
            },
        );
        tasks.insert(
            "no_newline".to_string(),
            Task {
                run: crate::config::TaskRun::Command(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf out".to_string(),
                ]),
                description: None,
                cwd: None,
                env: BTreeMap::new(),
                timeout: None,
                vars: BTreeMap::new(),
            },
        );
        tasks.insert(
            "sequence".to_string(),
            Task {
                run: crate::config::TaskRun::Sequence(vec![
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
                ]),
                description: None,
                cwd: None,
                env: BTreeMap::new(),
                timeout: None,
                vars: BTreeMap::new(),
            },
        );
        tasks.insert(
            "sequence_fails".to_string(),
            Task {
                run: crate::config::TaskRun::Sequence(vec![
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
                ]),
                description: None,
                cwd: None,
                env: BTreeMap::new(),
                timeout: None,
                vars: BTreeMap::new(),
            },
        );
        tasks.insert(
            "templated".to_string(),
            Task {
                run: crate::config::TaskRun::Command(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf '%s' \"$0\"".to_string(),
                    "{#value}".to_string(),
                ]),
                description: Some("Render one value".to_string()),
                cwd: None,
                env: BTreeMap::new(),
                timeout: None,
                vars: BTreeMap::from([(
                    "value".to_string(),
                    crate::config::TaskVar {
                        description: Some("Value to render".to_string()),
                        optional: false,
                    },
                )]),
            },
        );
        Config {
            ssh_target: "me@example".to_string(),
            local_root,
            remote_root: "/tmp/remote".into(),
            rpc_port: port,
            remote_bin_path: "~/.local/bin/sshpal-run".to_string(),
            sync_detection_timeout: DEFAULT_SYNC_DETECTION_TIMEOUT,
            tasks,
        }
    }

    fn loaded_config_for(port: u16) -> LoadedConfig {
        let config = config_for(port);
        let project_root = config.local_root.clone();
        let path = project_root.join(CONFIG_FILE_NAME);
        LoadedConfig {
            config,
            path,
            project_root,
        }
    }

    async fn collect_events(
        config: &Config,
        task: &str,
        vars: BTreeMap<String, String>,
        args: Vec<String>,
    ) -> Result<Vec<RpcEvent>> {
        let sync_token = if task == tasks::TASKS_HELP_NAME || task == tasks::CHECKHEALTH_NAME {
            None
        } else {
            let sync_token = format!("token-{task}");
            tokio::fs::write(health::sentinel_path(&config.local_root), format!("{sync_token}\n")).await?;
            Some(sync_token)
        };
        let url = format!("http://127.0.0.1:{}/run", config.rpc_port);
        let response = Client::builder()
            .build()?
            .post(url)
            .json(&RpcRequest {
                task: task.to_string(),
                vars,
                args,
                sync_token,
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

    async fn collect_task_help(config: &Config) -> Result<String> {
        let url = format!("http://127.0.0.1:{}/tasks-help", config.rpc_port);
        let response = Client::builder()
            .build()?
            .get(url)
            .send()
            .await
            .context("failed to contact local RPC daemon")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("RPC request failed: {status} {text}");
        }

        response.text().await.context("failed to read help response")
    }

    #[tokio::test]
    async fn rpc_serializes() {
        let req = RpcRequest {
            task: "test".to_string(),
            vars: BTreeMap::from([("name".to_string(), "value".to_string())]),
            args: vec!["a".to_string()],
            sync_token: Some("token".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let roundtrip: RpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, req);
    }

    #[tokio::test]
    async fn run_endpoint_streams_stdout_stderr_and_exit() {
        let port = 49001;
        let cfg = config_for(port);
        let handle = tokio::spawn(async move { serve(loaded_config_for(port)).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(&cfg, "test", BTreeMap::new(), Vec::new())
            .await
            .unwrap();
        handle.abort();
        assert_eq!(
            events,
            vec![
                stdout_event("out\n"),
                stderr_event("err\n"),
                RpcEvent::Exit { code: 7 },
            ]
        );
    }

    #[tokio::test]
    async fn unknown_task_is_rejected() {
        let port = 49002;
        let cfg = config_for(port);
        let handle = tokio::spawn(async move { serve(loaded_config_for(port)).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let err = collect_events(&cfg, "missing", BTreeMap::new(), Vec::new())
            .await
            .unwrap_err();
        handle.abort();
        assert!(err.to_string().contains("RPC request failed"));
    }

    #[tokio::test]
    async fn run_endpoint_executes_task_steps_sequentially() {
        let port = 49003;
        let cfg = config_for(port);
        let handle = tokio::spawn(async move { serve(loaded_config_for(port)).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(
            &cfg,
            "sequence",
            BTreeMap::new(),
            vec!["arg".to_string()],
        )
            .await
            .unwrap();
        handle.abort();
        assert_eq!(
            events,
            vec![
                stdout_event("first\n"),
                stdout_event("second arg\n"),
                RpcEvent::Exit { code: 0 },
            ]
        );
    }

    #[tokio::test]
    async fn run_endpoint_stops_on_first_failing_step() {
        let port = 49004;
        let cfg = config_for(port);
        let handle = tokio::spawn(async move { serve(loaded_config_for(port)).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(&cfg, "sequence_fails", BTreeMap::new(), Vec::new())
            .await
            .unwrap();
        handle.abort();
        assert_eq!(
            events,
            vec![
                stdout_event("before-fail\n"),
                stderr_event("boom\n"),
                RpcEvent::Exit { code: 9 },
            ]
        );
    }

    #[test]
    fn remote_helper_script_embeds_port_and_command_name() {
        let script = remote_helper_script(45678);
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("usage: sshpal-run <task> [name=value ...] [-- <args...>]"));
        assert!(script.contains("http://127.0.0.1:45678/run"));
        assert!(script.contains("http://127.0.0.1:45678/tasks-help"));
    }

    #[tokio::test]
    async fn run_endpoint_substitutes_client_vars() {
        let port = 49005;
        let cfg = config_for(port);
        let handle = tokio::spawn(async move { serve(loaded_config_for(port)).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(
            &cfg,
            "templated",
            BTreeMap::from([("value".to_string(), "hello world".to_string())]),
            Vec::new(),
        )
        .await
        .unwrap();
        handle.abort();
        assert_eq!(
            events,
            vec![
                stdout_event("hello world"),
                RpcEvent::Exit { code: 0 },
            ]
        );
    }

    #[tokio::test]
    async fn run_endpoint_preserves_output_without_trailing_newline() {
        let port = 49007;
        let cfg = config_for(port);
        let handle = tokio::spawn(async move { serve(loaded_config_for(port)).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let events = collect_events(&cfg, "no_newline", BTreeMap::new(), Vec::new())
            .await
            .unwrap();
        handle.abort();
        assert_eq!(events, vec![stdout_event("out"), RpcEvent::Exit { code: 0 }]);
    }

    #[tokio::test]
    async fn tasks_help_route_returns_remote_usage() {
        let port = 49006;
        let cfg = config_for(port);
        let handle = tokio::spawn(async move { serve(loaded_config_for(port)).await.unwrap() });
        sleep(Duration::from_millis(100)).await;
        let help = collect_task_help(&cfg).await.unwrap();
        handle.abort();
        assert!(help.contains("usage: sshpal-run templated value=<value> [-- <args...>]"));
        assert!(help.contains("Render one value"));
    }

    #[tokio::test]
    async fn serve_reports_actionable_error_when_port_is_taken() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = serve(loaded_config_for(port)).await.unwrap_err().to_string();
        assert!(err.contains("port already in use"));
        assert!(err.contains("shut down existing sshpal servers"));
        assert!(err.contains("rpc_port config option"));
    }

    fn stdout_event(chunk: &str) -> RpcEvent {
        RpcEvent::Stdout {
            chunk_b64: BASE64.encode(chunk.as_bytes()),
        }
    }

    fn stderr_event(chunk: &str) -> RpcEvent {
        RpcEvent::Stderr {
            chunk_b64: BASE64.encode(chunk.as_bytes()),
        }
    }
}
