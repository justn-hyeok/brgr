//! A private, pane-lifetime file bridge for brgr commands from sandboxed Codex.

use std::{
    env, fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Child, time::timeout};

use crate::{write_json_atomic, write_json_new};

pub const BRIDGE_DIR_ENV: &str = "BRGR_PLUGIN_BRIDGE_DIR";
pub const BRIDGE_HOST_HOME_ENV: &str = "BRGR_PLUGIN_HOST_HOME";
pub const BRIDGE_HOST_WORKSPACE_ENV: &str = "BRGR_PLUGIN_HOST_WORKSPACE";
const MAX_REQUEST_BYTES: u64 = 65_536;
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_BRIDGE_SECONDS: u64 = 7 * 24 * 3_600;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const KILL_WAIT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
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

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    id: String,
    code: i32,
    stdout: String,
    stderr: String,
}

struct ClaimGuard {
    path: PathBuf,
}

struct ProcessGroupGuard {
    pid: Option<u32>,
    armed: bool,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(pid) = self.pid
        {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn request_id() -> Result<String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    Ok(format!("{}-{nanos}-{sequence}", std::process::id()))
}

fn request_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.request.json"))
}

fn processing_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.processing"))
}

fn response_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.response.json"))
}

fn parse_bridge_id<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let id = name.strip_suffix(suffix)?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
        return None;
    }
    Some(id)
}

fn live_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        bail!("bridge file is not a bounded regular file");
    }
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    io::Read::take(file, max_bytes.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("bridge file is not a bounded regular file");
    }
    if !regular_file(path)? {
        bail!("bridge file is not a bounded regular file");
    }
    Ok(bytes)
}

pub async fn client(dir: &Path, timeout_seconds: u64) -> Result<()> {
    if !dir.is_absolute() || !live_directory(dir)? {
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
    if pretty_json_file_len(&request)? > MAX_REQUEST_BYTES {
        bail!("brgr Herdr bridge request exceeds 64 KiB");
    }
    write_json_new(&path, &request)?;
    let response = wait_for_response(dir, &request.id, timeout_seconds).await?;
    io::stdout().write_all(response.stdout.as_bytes())?;
    io::stderr().write_all(response.stderr.as_bytes())?;
    if response.code != 0 {
        std::process::exit(response.code.clamp(1, 125));
    }
    Ok(())
}

async fn wait_for_response(dir: &Path, id: &str, timeout_seconds: u64) -> Result<Response> {
    let request_file = request_path(dir, id);
    let processing_file = processing_path(dir, id);
    let response_file = response_path(dir, id);
    timeout(
        Duration::from_secs(timeout_seconds.saturating_add(10)),
        async {
            loop {
                if !live_directory(dir)? {
                    bail!("brgr Herdr bridge closed before responding");
                }
                if regular_file(&request_file)? || regular_file(&processing_file)? {
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
                if let Some(response) = load_response(&response_file, id)? {
                    return Ok(response);
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        },
    )
    .await
    .context("brgr Herdr bridge did not respond before its deadline")?
}

fn load_response(path: &Path, id: &str) -> Result<Option<Response>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            if metadata.len() > MAX_RESPONSE_BYTES {
                bail!("brgr Herdr bridge response exceeds 16 MiB");
            }
            let response: Response =
                serde_json::from_slice(&read_bounded_regular_file(path, MAX_RESPONSE_BYTES)?)?;
            if response.id != id {
                bail!("brgr Herdr bridge response identity differs");
            }
            Ok(Some(response))
        }
        Ok(_) => bail!("brgr Herdr bridge response is not a regular file"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub async fn serve(dir: PathBuf, executable: PathBuf, home: PathBuf, workspace: PathBuf) {
    loop {
        if let Err(error) = serve_ready(&dir, &executable, &home, &workspace).await {
            eprintln!("brgr Herdr bridge: {error:#}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn serve_ready(dir: &Path, executable: &Path, home: &Path, workspace: &Path) -> Result<()> {
    let mut jobs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        if let Some(id) = parse_bridge_id(name, ".processing") {
            jobs.push((id.to_owned(), path, true));
        } else if let Some(id) = parse_bridge_id(name, ".request.json") {
            jobs.push((id.to_owned(), path, false));
        } else if name.ends_with(".processing") || name.ends_with(".request.json") {
            let _ = fs::remove_file(&path);
        }
    }
    jobs.sort_by(|left, right| left.0.cmp(&right.0).then(right.2.cmp(&left.2)));
    for (id, path, stale_processing) in jobs {
        if let Err(error) = complete_job(
            dir,
            executable,
            home,
            workspace,
            &id,
            path,
            stale_processing,
        )
        .await
        {
            eprintln!("brgr Herdr bridge: {error:#}");
        }
    }
    Ok(())
}

fn claim_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.claim"))
}

fn try_claim(dir: &Path, id: &str) -> bool {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(claim_path(dir, id))
        .is_ok()
}

async fn complete_job(
    dir: &Path,
    executable: &Path,
    home: &Path,
    workspace: &Path,
    id: &str,
    path: PathBuf,
    stale_processing: bool,
) -> Result<()> {
    let processing = processing_path(dir, id);
    if stale_processing && regular_file(&request_path(dir, id))? {
        return Ok(());
    }
    if stale_processing && regular_file(&response_path(dir, id))? {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    if !try_claim(dir, id) {
        return Ok(());
    }
    let _claim = ClaimGuard {
        path: claim_path(dir, id),
    };
    let claimed = if stale_processing {
        path
    } else if fs::rename(&path, &processing).is_err() {
        return Ok(());
    } else {
        processing
    };
    let response = match read_request(&claimed, id) {
        Ok(request) => run_request(request, executable, home, workspace).await,
        Err(error) => Response {
            id: id.to_owned(),
            code: 1,
            stdout: String::new(),
            stderr: format!("brgr Herdr bridge rejected request: {error:#}\n"),
        },
    };
    let response = bounded_response(response)?;
    write_json_atomic(&response_path(dir, id), &response)?;
    let _ = fs::remove_file(&claimed);
    Ok(())
}

fn bounded_response(response: Response) -> Result<Response> {
    if pretty_json_file_len(&response)? <= MAX_RESPONSE_BYTES {
        return Ok(response);
    }
    Ok(Response {
        id: response.id,
        code: 1,
        stdout: String::new(),
        stderr: "brgr Herdr bridge encoded response exceeded 16 MiB\n".to_owned(),
    })
}

fn pretty_json_file_len(value: &impl Serialize) -> Result<u64> {
    let encoded = u64::try_from(serde_json::to_vec_pretty(value)?.len())?;
    encoded
        .checked_add(1)
        .context("bridge JSON file length overflow")
}

fn read_request(path: &Path, id: &str) -> Result<Request> {
    let request: Request =
        serde_json::from_slice(&read_bounded_regular_file(path, MAX_REQUEST_BYTES)?)?;
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

async fn run_request(
    request: Request,
    executable: &Path,
    home: &Path,
    workspace: &Path,
) -> Response {
    let mut command = tokio::process::Command::new(executable);
    command
        .args(&request.args)
        .current_dir(&request.cwd)
        .env("BRGR_HOME", home)
        .env(BRIDGE_HOST_HOME_ENV, home)
        .env(BRIDGE_HOST_WORKSPACE_ENV, workspace)
        .env_remove(BRIDGE_DIR_ENV)
        .env_remove("CODEX_THREAD_ID")
        .env_remove("BRGR_SESSION_ID")
        .env_remove("BRGR_OWNER_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    for (name, value) in [
        ("CODEX_THREAD_ID", request.codex_thread_id.as_ref()),
        ("BRGR_SESSION_ID", request.brgr_session_id.as_ref()),
        ("BRGR_OWNER_ID", request.brgr_owner_id.as_ref()),
    ] {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Response {
                id: request.id,
                code: 1,
                stdout: String::new(),
                stderr: format!("brgr Herdr bridge execution failed: {error}\n"),
            };
        }
    };
    let pid = child.id();
    let mut process_group = ProcessGroupGuard::new(pid);
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let total = AtomicUsize::new(0);
    let capture = async {
        let stdout = read_capped_pipe(&mut stdout_pipe, &total, pid);
        let stderr = read_capped_pipe(&mut stderr_pipe, &total, pid);
        let status = child.wait();
        tokio::join!(status, stdout, stderr)
    };
    let captured = timeout(Duration::from_secs(request.timeout_seconds), capture).await;
    let (code, stdout, stderr) = if let Ok((status, stdout, stderr)) = captured {
        match (status, stdout, stderr) {
            (Ok(status), Ok(stdout), Ok(stderr))
                if total.load(Ordering::Relaxed) <= MAX_OUTPUT_BYTES =>
            {
                (
                    status.code().unwrap_or(1),
                    String::from_utf8_lossy(&stdout).into_owned(),
                    String::from_utf8_lossy(&stderr).into_owned(),
                )
            }
            (Ok(_), Ok(_), Ok(_)) => {
                stop_child(&mut child, pid).await;
                (
                    1,
                    String::new(),
                    "brgr Herdr bridge output exceeded 8 MiB\n".to_owned(),
                )
            }
            (Err(error), _, _) => (
                1,
                String::new(),
                format!("brgr Herdr bridge execution failed: {error}\n"),
            ),
            (_, Err(error), _) | (_, _, Err(error)) => (
                1,
                String::new(),
                format!("brgr Herdr bridge output read failed: {error}\n"),
            ),
        }
    } else {
        stop_child(&mut child, pid).await;
        (
            1,
            String::new(),
            "brgr Herdr bridge command timed out\n".to_owned(),
        )
    };
    process_group.disarm();
    Response {
        id: request.id,
        code,
        stdout,
        stderr,
    }
}

async fn read_capped_pipe<T: AsyncReadExt + Unpin>(
    pipe: &mut Option<T>,
    total: &AtomicUsize,
    pid: Option<u32>,
) -> io::Result<Vec<u8>> {
    let Some(pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    let mut buf = vec![0_u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Err(error) => return Err(error),
            Ok(n) => {
                let previous = total.fetch_add(n, Ordering::Relaxed);
                if previous.saturating_add(n) > MAX_OUTPUT_BYTES {
                    kill_process_group(pid).await;
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
            }
        }
    }
    Ok(bytes)
}

async fn stop_child(child: &mut Child, pid: Option<u32>) {
    kill_process_group(pid).await;
    let _ = child.start_kill();
    let _ = timeout(KILL_WAIT, child.wait()).await;
}

async fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let _ = tokio::process::Command::new("/bin/kill")
        .args(["-KILL", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use tempfile::TempDir;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn sample_request(id: &str, cwd: &Path, args: &[&str], timeout_seconds: u64) -> Request {
        Request {
            id: id.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: cwd.to_path_buf(),
            codex_thread_id: None,
            brgr_session_id: None,
            brgr_owner_id: None,
            timeout_seconds,
        }
    }

    fn process_is_live(pid: u32) -> bool {
        let output = std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "state="])
            .output()
            .unwrap();
        if !output.status.success() {
            return false;
        }
        let state = String::from_utf8_lossy(&output.stdout);
        let state = state.trim();
        matches!(state.chars().next(), Some(kind) if kind != 'Z')
    }

    #[tokio::test]
    async fn malformed_request_files_are_dropped_without_blocking_valid_work() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("evil.request.json"), b"not-json").unwrap();
        fs::write(dir.join(".request.json"), b"{}").unwrap();
        let request = sample_request("1-2-3", root.path(), &["ok"], 5);
        write_json_new(&request_path(&dir, &request.id), &request).unwrap();
        serve_ready(&dir, Path::new("/bin/echo"), root.path(), root.path())
            .await
            .unwrap();
        assert!(!dir.join("evil.request.json").exists());
        assert!(!dir.join(".request.json").exists());
        let response: Response =
            serde_json::from_slice(&fs::read(response_path(&dir, "1-2-3")).unwrap()).unwrap();
        assert_eq!(response.id, "1-2-3");
        assert_eq!(response.code, 0);
        assert_eq!(response.stdout.trim(), "ok");
    }

    #[tokio::test]
    async fn concurrent_servers_claim_a_request_once() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        let counter = root.path().join("count");
        let script = write_script(root.path(), "once.sh", "#!/bin/sh\necho x >> \"$1\"\n");
        let request = sample_request("9-9-9", root.path(), &[counter.to_str().unwrap()], 5);
        write_json_new(&request_path(&dir, &request.id), &request).unwrap();
        let first = serve_ready(&dir, &script, root.path(), root.path());
        let second = serve_ready(&dir, &script, root.path(), root.path());
        let (left, right) = tokio::join!(first, second);
        left.unwrap();
        right.unwrap();
        assert_eq!(fs::read_to_string(counter).unwrap(), "x\n");
        let response: Response =
            serde_json::from_slice(&fs::read(response_path(&dir, "9-9-9")).unwrap()).unwrap();
        assert_eq!(response.id, "9-9-9");
        assert_eq!(response.code, 0);
    }

    #[tokio::test]
    async fn planted_response_is_ignored_until_the_claim_completes() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        let request = sample_request("4-5-6", root.path(), &["real"], 5);
        write_json_new(&request_path(&dir, &request.id), &request).unwrap();
        write_json_atomic(
            &response_path(&dir, &request.id),
            &Response {
                id: request.id.clone(),
                code: 0,
                stdout: "planted\n".to_owned(),
                stderr: String::new(),
            },
        )
        .unwrap();
        let waiter = wait_for_response(&dir, &request.id, 2);
        let server = async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            serve_ready(&dir, Path::new("/bin/echo"), root.path(), root.path()).await
        };
        let (response, host) = tokio::join!(waiter, server);
        host.unwrap();
        let response = response.unwrap();
        assert_eq!(response.id, "4-5-6");
        assert_eq!(response.stdout.trim(), "real");
    }

    #[tokio::test]
    async fn response_identity_mismatch_fails_closed() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        write_json_atomic(
            &response_path(&dir, "7-7-7"),
            &Response {
                id: "other".to_owned(),
                code: 0,
                stdout: "nope\n".to_owned(),
                stderr: String::new(),
            },
        )
        .unwrap();
        let error = wait_for_response(&dir, "7-7-7", 1).await.unwrap_err();
        assert!(error.to_string().contains("response identity differs"));
    }

    #[tokio::test]
    async fn timed_out_child_and_group_are_killed() {
        let root = TempDir::new().unwrap();
        let pid_file = root.path().join("child.pid");
        let script = write_script(
            root.path(),
            "hang.sh",
            "#!/bin/sh\necho $$ > \"$1\"\nexec /bin/sleep 30\n",
        );
        let response = run_request(
            sample_request("8-8-8", root.path(), &[pid_file.to_str().unwrap()], 1),
            &script,
            root.path(),
            root.path(),
        )
        .await;
        assert_eq!(response.code, 1);
        assert!(response.stderr.contains("timed out"));
        let pid: u32 = {
            let mut last = String::new();
            for _ in 0..100 {
                match fs::read_to_string(&pid_file) {
                    Ok(contents) if !contents.trim().is_empty() => {
                        last = contents;
                        break;
                    }
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
            last.trim().parse().unwrap()
        };
        assert!(!process_is_live(pid));
    }

    #[tokio::test]
    async fn aborting_serve_kills_an_in_flight_child() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        let leader_pid_file = root.path().join("pane.pid");
        let child_pid_file = root.path().join("pane-child.pid");
        let script = write_script(
            root.path(),
            "pane-hang.sh",
            "#!/bin/sh\necho $$ > \"$1\"\n/bin/sleep 30 &\necho $! > \"$2\"\nwait\n",
        );
        let request = sample_request(
            "3-3-3",
            root.path(),
            &[
                leader_pid_file.to_str().unwrap(),
                child_pid_file.to_str().unwrap(),
            ],
            20,
        );
        write_json_new(&request_path(&dir, &request.id), &request).unwrap();
        let serve = tokio::spawn(serve(
            dir.clone(),
            script,
            root.path().to_path_buf(),
            root.path().to_path_buf(),
        ));
        let (leader_pid, child_pid) = loop {
            if let (Ok(leader), Ok(child)) = (
                fs::read_to_string(&leader_pid_file),
                fs::read_to_string(&child_pid_file),
            ) && let (Ok(leader), Ok(child)) =
                (leader.trim().parse::<u32>(), child.trim().parse::<u32>())
            {
                break (leader, child);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        };
        serve.abort();
        let _ = serve.await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!process_is_live(leader_pid));
        assert!(!process_is_live(child_pid));
    }

    #[tokio::test]
    async fn symlink_request_is_rejected_without_following() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        let outside = root.path().join("outside.json");
        let request = sample_request("2-2-2", root.path(), &["secret"], 5);
        write_json_new(&outside, &request).unwrap();
        std::os::unix::fs::symlink(&outside, request_path(&dir, "2-2-2")).unwrap();
        serve_ready(&dir, Path::new("/bin/echo"), root.path(), root.path())
            .await
            .unwrap();
        let response: Response =
            serde_json::from_slice(&fs::read(response_path(&dir, "2-2-2")).unwrap()).unwrap();
        assert_eq!(response.code, 1);
        assert!(response.stderr.contains("bounded regular file"));
        assert!(!response.stdout.contains("secret"));
    }

    #[tokio::test]
    async fn stale_processing_with_a_response_is_not_replayed() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        let counter = root.path().join("replay");
        let script = write_script(root.path(), "replay.sh", "#!/bin/sh\necho x >> \"$1\"\n");
        fs::write(&counter, "x\n").unwrap();
        let request = sample_request("5-5-5", root.path(), &[counter.to_str().unwrap()], 5);
        write_json_new(&processing_path(&dir, &request.id), &request).unwrap();
        write_json_atomic(
            &response_path(&dir, &request.id),
            &Response {
                id: request.id.clone(),
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        )
        .unwrap();
        serve_ready(&dir, &script, root.path(), root.path())
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(counter).unwrap(), "x\n");
        assert!(!processing_path(&dir, "5-5-5").exists());
    }

    #[tokio::test]
    async fn output_larger_than_a_pipe_buffer_completes_under_the_cap() {
        let root = TempDir::new().unwrap();
        let script = write_script(
            root.path(),
            "wide.sh",
            "#!/bin/sh\ndd if=/dev/zero bs=1024 count=256 2>/dev/null | tr '\\0' 'x'\n",
        );
        let started = std::time::Instant::now();
        let response = run_request(
            sample_request("11-11-11", root.path(), &[], 5),
            &script,
            root.path(),
            root.path(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(response.code, 0);
        assert_eq!(response.stdout.len(), 256 * 1024);
        assert!(response.stdout.bytes().all(|byte| byte == b'x'));
    }

    #[tokio::test]
    async fn oversized_output_kills_the_process_group() {
        let root = TempDir::new().unwrap();
        let pid_file = root.path().join("wide.pid");
        let script = write_script(
            root.path(),
            "overflow.sh",
            "#!/bin/sh\necho $$ > \"$1\"\nexec dd if=/dev/zero bs=1048576 count=32\n",
        );
        let response = run_request(
            sample_request("12-12-12", root.path(), &[pid_file.to_str().unwrap()], 20),
            &script,
            root.path(),
            root.path(),
        )
        .await;
        assert_eq!(response.code, 1);
        assert!(response.stderr.contains("output exceeded 8 MiB"));
        let pid: u32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!process_is_live(pid));
    }

    #[tokio::test]
    async fn claim_is_released_when_response_write_fails() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("bridge");
        fs::create_dir(&dir).unwrap();
        let request = sample_request("6-6-6", root.path(), &["ok"], 5);
        write_json_new(&request_path(&dir, &request.id), &request).unwrap();
        fs::create_dir(response_path(&dir, "6-6-6")).unwrap();
        serve_ready(&dir, Path::new("/bin/echo"), root.path(), root.path())
            .await
            .unwrap();
        assert!(!claim_path(&dir, "6-6-6").exists());
    }

    #[test]
    fn encoded_response_limit_returns_a_small_explicit_error() {
        let response = bounded_response(Response {
            id: "10-10-10".to_owned(),
            code: 0,
            stdout: "\0".repeat(3 * 1024 * 1024),
            stderr: String::new(),
        })
        .unwrap();
        assert_eq!(response.id, "10-10-10");
        assert_eq!(response.code, 1);
        assert!(response.stdout.is_empty());
        assert!(response.stderr.contains("encoded response exceeded"));
        assert!(pretty_json_file_len(&response).unwrap() <= MAX_RESPONSE_BYTES);
    }
}
