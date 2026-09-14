//! A private, pane-lifetime file bridge for brgr commands from sandboxed Codex.

use std::{
    env, fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::timeout};

use crate::{write_json_atomic, write_json_new};

pub const BRIDGE_DIR_ENV: &str = "BRGR_PLUGIN_BRIDGE_DIR";
const MAX_REQUEST_BYTES: u64 = 65_536;
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_BRIDGE_SECONDS: u64 = 7 * 24 * 3_600;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
struct Request {
    id: String,
    args: Vec<String>,
    cwd: PathBuf,
    codex_thread_id: Option<String>,
    brgr_session_id: Option<String>,
    brgr_owner_id: Option<String>,
    timeout_seconds: u64,
}

#[derive(Serialize, Deserialize)]
struct Response {
    id: String,
    code: i32,
    stdout: String,
    stderr: String,
}

fn request_id() -> Result<String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    Ok(format!("{}-{nanos}-{sequence}", std::process::id()))
}

fn request_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.request.json"))
}

fn response_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.response.json"))
}

pub async fn client(dir: &Path, timeout_seconds: u64) -> Result<()> {
    if !dir.is_absolute() {
        bail!("brgr Herdr bridge directory is unavailable");
    }
    let metadata =
        fs::symlink_metadata(dir).context("brgr Herdr bridge directory is unavailable")?;
    if !metadata.file_type().is_dir() {
        bail!("brgr Herdr bridge directory is unavailable");
    }
    if timeout_seconds == 0 || timeout_seconds > MAX_BRIDGE_SECONDS {
        bail!("brgr Herdr bridge deadline must be within seven days");
    }
    let args = env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| anyhow::anyhow!("brgr argument is not UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    let request = Request {
        id: request_id()?,
        args,
        cwd: env::current_dir()?,
        codex_thread_id: env::var("CODEX_THREAD_ID").ok(),
        brgr_session_id: env::var("BRGR_SESSION_ID").ok(),
        brgr_owner_id: env::var("BRGR_OWNER_ID").ok(),
        timeout_seconds,
    };
    let path = request_path(dir, &request.id);
    if serde_json::to_vec(&request)?.len() as u64 > MAX_REQUEST_BYTES {
        bail!("brgr Herdr bridge request exceeds 64 KiB");
    }
    write_json_new(&path, &request)?;
    let response_file = response_path(dir, &request.id);
    let response = timeout(
        Duration::from_secs(timeout_seconds.saturating_add(10)),
        async {
            loop {
                match fs::symlink_metadata(&response_file) {
                    Ok(metadata) if metadata.file_type().is_file() => {
                        if metadata.len() > MAX_RESPONSE_BYTES {
                            bail!("brgr Herdr bridge response exceeds 16 MiB");
                        }
                        let response: Response =
                            serde_json::from_slice(&fs::read(&response_file)?)?;
                        if response.id != request.id {
                            bail!("brgr Herdr bridge response identity differs");
                        }
                        return Ok(response);
                    }
                    Ok(_) => bail!("brgr Herdr bridge response is not a regular file"),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                if !dir.is_dir() {
                    bail!("brgr Herdr bridge closed before responding");
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        },
    )
    .await
    .context("brgr Herdr bridge did not respond before its deadline")??;
    io::stdout().write_all(response.stdout.as_bytes())?;
    io::stderr().write_all(response.stderr.as_bytes())?;
    if response.code != 0 {
        std::process::exit(response.code.clamp(1, 125));
    }
    Ok(())
}

pub async fn serve(dir: PathBuf, executable: PathBuf, home: PathBuf) {
    loop {
        if let Err(error) = serve_ready(&dir, &executable, &home).await {
            eprintln!("brgr Herdr bridge: {error:#}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn serve_ready(dir: &Path, executable: &Path, home: &Path) -> Result<()> {
    let mut pending = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".request.json")
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    pending.sort();
    for path in pending {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("bridge request path is not UTF-8")?;
        let id = name
            .strip_suffix(".request.json")
            .context("bridge request filename is malformed")?;
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
            bail!("bridge request id is malformed");
        }
        let processing = dir.join(format!("{id}.processing"));
        if fs::rename(&path, &processing).is_err() {
            continue;
        }
        let response = match read_request(&processing, id) {
            Ok(request) => run_request(request, executable, home).await,
            Err(error) => Response {
                id: id.to_owned(),
                code: 1,
                stdout: String::new(),
                stderr: format!("brgr Herdr bridge rejected request: {error:#}\n"),
            },
        };
        write_json_atomic(&response_path(dir, id), &response)?;
        let _ = fs::remove_file(processing);
    }
    Ok(())
}

fn read_request(path: &Path, id: &str) -> Result<Request> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_REQUEST_BYTES {
        bail!("bridge request is not a bounded regular file");
    }
    let request: Request = serde_json::from_slice(&fs::read(path)?)?;
    if request.id != id
        || request.args.is_empty()
        || !request.cwd.is_absolute()
        || request.timeout_seconds == 0
        || request.timeout_seconds > MAX_BRIDGE_SECONDS
    {
        bail!("bridge request identity, command, or cwd is invalid");
    }
    if request.args.iter().map(String::len).sum::<usize>() > 65_536 {
        bail!("bridge command arguments exceed 64 KiB");
    }
    Ok(request)
}

async fn run_request(request: Request, executable: &Path, home: &Path) -> Response {
    let mut command = Command::new(executable);
    command
        .args(&request.args)
        .current_dir(&request.cwd)
        .env("BRGR_HOME", home)
        .env_remove(BRIDGE_DIR_ENV)
        .env_remove("CODEX_THREAD_ID")
        .env_remove("BRGR_SESSION_ID")
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (name, value) in [
        ("CODEX_THREAD_ID", request.codex_thread_id.as_ref()),
        ("BRGR_SESSION_ID", request.brgr_session_id.as_ref()),
        ("BRGR_OWNER_ID", request.brgr_owner_id.as_ref()),
    ] {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let outcome = timeout(
        Duration::from_secs(request.timeout_seconds),
        command.output(),
    )
    .await;
    let (code, stdout, stderr) = match outcome {
        Ok(Ok(output))
            if output.stdout.len().saturating_add(output.stderr.len()) <= 8 * 1024 * 1024 =>
        {
            (
                output.status.code().unwrap_or(1),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        }
        Ok(Ok(_)) => (
            1,
            String::new(),
            "brgr Herdr bridge output exceeded 8 MiB\n".to_owned(),
        ),
        Ok(Err(error)) => (
            1,
            String::new(),
            format!("brgr Herdr bridge execution failed: {error}\n"),
        ),
        Err(_) => (
            1,
            String::new(),
            "brgr Herdr bridge command timed out\n".to_owned(),
        ),
    };
    Response {
        id: request.id,
        code,
        stdout,
        stderr,
    }
}
