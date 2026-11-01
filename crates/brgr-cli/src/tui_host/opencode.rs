//! Select and read back an `OpenCode` 2 session before opening its private TUI.
//! A temporary private server settles the catalog and persists the session;
//! it is stopped before the TUI resumes the session in its own private server.
use super::NativeLaunch;
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::process::CommandExt as _,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT: u64 = 1024 * 1024;
const API_DEADLINE: Duration = Duration::from_secs(8);

fn model_ref(selector: &str) -> Result<Value> {
    if selector.len() > 512
        || selector
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("OpenCode model must be provider/model#variant");
    }
    let (base, variant) = match selector.split_once('#') {
        Some((base, variant)) if !variant.is_empty() && !variant.contains('#') => {
            (base, Some(variant))
        }
        Some(_) => bail!("OpenCode model has an invalid variant"),
        None => (selector, None),
    };
    let (provider, id) = base
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .context("OpenCode model must be provider/model#variant")?;
    let mut model = json!({"providerID": provider, "id": id});
    if let Some(variant) = variant {
        model["variant"] = json!(variant);
    }
    Ok(model)
}

fn check_args(argv: &[String]) -> Result<()> {
    if !argv.iter().any(|arg| arg == "--standalone") {
        bail!("OpenCode session model requires a private standalone TUI");
    }
    for arg in argv {
        let flag = arg.split('=').next().unwrap_or(arg);
        if matches!(flag, "--session" | "--continue" | "--server" | "--model")
            || arg.starts_with("-s") && !arg.starts_with("--")
            || arg.starts_with("-c") && !arg.starts_with("--")
            || arg.starts_with("-m") && !arg.starts_with("--")
        {
            bail!("OpenCode session model conflicts with a calling session/server/model option");
        }
    }
    Ok(())
}

fn verified_session(response: &Value, model: &Value, launch: &NativeLaunch) -> Result<String> {
    let data = &response["data"];
    let actual = &data["model"];
    if actual["providerID"] != model["providerID"]
        || actual["id"] != model["id"]
        || model
            .get("variant")
            .is_some_and(|v| actual["variant"] != *v)
        || data["location"]["directory"].as_str() != launch.workspace.to_str()
    {
        bail!("OpenCode did not retain the requested session model/variant/location");
    }
    let id = data["id"]
        .as_str()
        .context("OpenCode session id is absent")?;
    if !id.starts_with("ses_")
        || id.len() > 128
        || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        bail!("OpenCode returned an invalid session id");
    }
    Ok(id.to_owned())
}

pub(super) fn prepare(launch: &mut NativeLaunch, selector: &str) -> Result<()> {
    check_args(&launch.argv)?;
    let model = model_ref(selector)?;
    let server = Server::start(launch)?;
    let location = format!("location.directory={}", launch.workspace.display());
    let started = Instant::now();
    loop {
        let catalog = api(launch, &server, &["model.list", "--param", &location])?;
        if catalog["location"]["directory"].as_str() != launch.workspace.to_str() {
            bail!("OpenCode returned a catalog for a different task location");
        }
        if catalog_contains(&catalog, &model)? {
            break;
        }
        if started.elapsed() >= Duration::from_secs(5) {
            bail!("OpenCode requested model is not available in the task location");
        }
        // The first catalog response can precede plugin settlement. Reuse the
        // same private server rather than restarting that settlement each time.
        thread::sleep(Duration::from_millis(250));
    }
    let body = json!({"model": model, "location": {"directory": launch.workspace}}).to_string();
    let response = api(launch, &server, &["POST", "/api/session", "--data", &body])?;
    let id = verified_session(&response, &model, launch)?;
    let response = api(launch, &server, &["GET", &format!("/api/session/{id}")])?;
    if verified_session(&response, &model, launch)? != id {
        bail!("OpenCode session read-back returned a different session");
    }
    launch.argv.extend(["--session".to_owned(), id]);
    Ok(())
}

fn catalog_contains(catalog: &Value, requested: &Value) -> Result<bool> {
    let models = catalog["data"]
        .as_array()
        .context("OpenCode returned an invalid model catalog")?;
    let Some(model) = models.iter().find(|model| {
        model["providerID"] == requested["providerID"] && model["id"] == requested["id"]
    }) else {
        return Ok(false);
    };
    if let Some(variant) = requested.get("variant")
        && !model["variants"]
            .as_array()
            .is_some_and(|variants| variants.iter().any(|entry| entry["id"] == *variant))
    {
        bail!("OpenCode requested variant is not available for this model");
    }
    Ok(true)
}

struct Server {
    _child: ApiChild,
    url: String,
    password: String,
    log: tempfile::NamedTempFile,
    deadline: Instant,
}

impl Server {
    fn start(launch: &NativeLaunch) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let password = uuid::Uuid::new_v4().to_string();
        let log = tempfile::NamedTempFile::new()?;
        let mut child = ApiChild(
            Command::new(&launch.executable)
                .args(["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"])
                .current_dir(&launch.workspace)
                .env_clear()
                .envs(&launch.environment)
                .env("OPENCODE_PASSWORD", &password)
                // OpenCode's stdio server exits on EOF. Keep this pipe owned
                // by the host so cancellation or host death stops the server
                // even when Rust's drop guards cannot run.
                .stdin(Stdio::piped())
                .stdout(log.reopen()?)
                .stderr(log.reopen()?)
                .process_group(0)
                .spawn()
                .context("could not start private OpenCode catalog server")?,
        );
        let started = Instant::now();
        loop {
            if log.as_file().metadata()?.len() > MAX_OUTPUT {
                bail!("OpenCode catalog server exceeded its output limit");
            }
            let output = fs::read_to_string(log.path())?;
            let url = output
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find_map(|record| record["url"].as_str().map(str::to_owned));
            if let Some(url) = url {
                let port = url
                    .strip_prefix("http://127.0.0.1:")
                    .and_then(|port| port.parse::<u16>().ok());
                if port.is_none_or(|port| port == 0) {
                    bail!("OpenCode catalog server returned invalid connection details");
                }
                return Ok(Self {
                    url,
                    password,
                    _child: child,
                    log,
                    deadline,
                });
            }
            if child.0.try_wait()?.is_some() || started.elapsed() >= API_DEADLINE {
                bail!("OpenCode catalog server did not become ready");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Guard both the CLI and its private server on errors or a deadline. Output
/// stays in private temporary files and raw error output is never disclosed.
struct ApiChild(std::process::Child);

impl Drop for ApiChild {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.0.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn api(launch: &NativeLaunch, server: &Server, args: &[&str]) -> Result<Value> {
    if Instant::now() >= server.deadline {
        bail!("OpenCode session preparation timed out");
    }
    let stdout = tempfile::NamedTempFile::new()?;
    let stderr = tempfile::NamedTempFile::new()?;
    let mut child = ApiChild(
        Command::new(&launch.executable)
            .args(["api", "--server", &server.url])
            .args(args)
            .current_dir(&launch.workspace)
            .env_clear()
            .envs(&launch.environment)
            // OpenCode 2 remote clients read this password from the process
            // environment. Never put it in argv, persisted launch files or logs.
            .env("OPENCODE_PASSWORD", &server.password)
            .stdin(Stdio::null())
            .stdout(stdout.reopen()?)
            .stderr(stderr.reopen()?)
            .process_group(0)
            .spawn()
            .context("could not start OpenCode session API")?,
    );
    let started = Instant::now();
    loop {
        if server.log.as_file().metadata()?.len() > MAX_OUTPUT {
            bail!("OpenCode catalog server exceeded its output limit");
        }
        if stdout.as_file().metadata()?.len() > MAX_OUTPUT
            || stderr.as_file().metadata()?.len() > MAX_OUTPUT
        {
            bail!("OpenCode session API exceeded its output limit");
        }
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                bail!("OpenCode session API failed (exit {:?})", status.code());
            }
            return serde_json::from_slice(&fs::read(stdout.path())?)
                .context("OpenCode session API returned invalid JSON");
        }
        if started.elapsed() >= API_DEADLINE || Instant::now() >= server.deadline {
            bail!("OpenCode session API timed out");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, path::PathBuf};

    fn launch() -> NativeLaunch {
        NativeLaunch {
            executable: PathBuf::from("/bin/false"),
            digest: String::new(),
            argv: vec!["--standalone".into(), "--auto".into()],
            workspace: PathBuf::from("/task"),
            environment: BTreeMap::new(),
            pane_environment: vec![],
            state: PathBuf::new(),
            opencode_model: Some("openai/gpt-5.2#high".into()),
        }
    }

    #[test]
    fn preserves_provider_model_and_variant_without_changing_global_config() {
        assert_eq!(
            model_ref("openai/gpt-5.2#high").unwrap(),
            json!({"providerID":"openai", "id":"gpt-5.2", "variant":"high"})
        );
        assert_eq!(
            model_ref("custom/team/model").unwrap(),
            json!({"providerID":"custom", "id":"team/model"})
        );
        for invalid in ["model", "/model", "provider/", "p/m#", "p/m#a#b", "p/m\n"] {
            assert!(model_ref(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn refuses_fallback_variant_loss_wrong_location_or_unsafe_session_id() {
        let launch = launch();
        let model = model_ref("openai/gpt-5.2#high").unwrap();
        let response = json!({"data":{"id":"ses_test", "model":model,
            "location":{"directory":"/task"}}});
        assert_eq!(
            verified_session(&response, &model, &launch).unwrap(),
            "ses_test"
        );
        for (path, wrong) in [
            ("/data/model/id", json!("fallback")),
            ("/data/model/providerID", json!("other")),
            ("/data/model/variant", json!("default")),
            ("/data/location/directory", json!("/other")),
            ("/data/id", json!("ses_../../other")),
        ] {
            let mut altered = response.clone();
            *altered.pointer_mut(path).unwrap() = wrong;
            assert!(verified_session(&altered, &model, &launch).is_err());
        }
    }

    #[test]
    fn refuses_existing_session_or_remote_server_overrides() {
        for arg in [
            "--session=ses_other",
            "-sother",
            "--continue",
            "-c",
            "--server=x",
            "--model=x",
        ] {
            let mut launch = launch();
            launch.argv.push(arg.into());
            assert!(check_args(&launch.argv).is_err());
        }
        assert!(check_args(&["--auto".into()]).is_err());
        assert!(check_args(&launch().argv).is_ok());
    }

    #[test]
    fn failed_api_never_opens_an_unselected_session() {
        let mut launch = launch();
        assert!(prepare(&mut launch, "openai/gpt-5.2#high").is_err());
        assert_eq!(launch.argv, ["--standalone", "--auto"]);
    }

    #[test]
    fn catalog_requires_the_exact_model_and_variant() {
        let catalog = json!({"data":[{"providerID":"openai", "id":"gpt-5.2",
            "variants":[{"id":"high"}]}]});
        assert!(catalog_contains(&catalog, &model_ref("openai/gpt-5.2#high").unwrap()).unwrap());
        assert!(!catalog_contains(&catalog, &model_ref("other/gpt-5.2").unwrap()).unwrap());
        assert!(catalog_contains(&catalog, &model_ref("openai/gpt-5.2#max").unwrap()).is_err());
        assert!(catalog_contains(&json!({}), &model_ref("openai/gpt-5.2").unwrap()).is_err());
    }

    fn fixture() -> (tempfile::TempDir, NativeLaunch) {
        let root = tempfile::tempdir().unwrap();
        let mut launch = launch();
        launch.workspace = root.path().to_owned();
        let response = json!({"data":{"id":"ses_test",
            "model":{"providerID":"openai", "id":"gpt-5.2", "variant":"high"},
            "location":{"directory":root.path()}}});
        for file in ["created.json", "readback.json"] {
            fs::write(root.path().join(file), response.to_string()).unwrap();
        }
        fs::write(
            root.path().join("catalog.json"),
            json!({
                "location":{"directory":root.path()},
                "data":[{"providerID":"openai", "id":"gpt-5.2", "variants":[{"id":"high"}]}]
            })
            .to_string(),
        )
        .unwrap();
        launch.executable = root.path().join("opencode");
        crate::fast_script::write_fast_script(
            &launch.executable,
            r#"#!/bin/sh
if [ "$1" = serve ]; then
  echo $$ > server.pid
  printf '%s' "$OPENCODE_PASSWORD" > fixture-password
  echo '{"url":"http://127.0.0.1:12345"}'
  exec /bin/sleep 30
fi
[ "$OPENCODE_PASSWORD" = "$(/bin/cat fixture-password)" ] || exit 1
[ "$1" = api ] && [ "$2" = --server ] || exit 2
case "$4" in
  model.list) /bin/cat catalog.json ;;
  POST) printf '%s\n' "$@" > create-args; /bin/cat created.json ;;
  GET) /bin/cat readback.json ;;
  *) exit 3 ;;
esac
"#,
        );
        (root, launch)
    }

    fn server_stopped(root: &std::path::Path) {
        let pid = fs::read_to_string(root.join("server.pid")).unwrap();
        assert!(
            !Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn prepares_a_verified_session_then_stops_the_temporary_server() {
        let (root, mut launch) = fixture();
        prepare(&mut launch, "openai/gpt-5.2#high").unwrap();
        assert_eq!(
            launch.argv,
            ["--standalone", "--auto", "--session", "ses_test"]
        );
        let args = fs::read_to_string(root.path().join("create-args")).unwrap();
        let body: Value = serde_json::from_str(args.lines().last().unwrap()).unwrap();
        assert_eq!(body["model"], model_ref("openai/gpt-5.2#high").unwrap());
        assert_eq!(body["location"]["directory"], json!(root.path()));
        let password = fs::read_to_string(root.path().join("fixture-password")).unwrap();
        assert!(!args.contains(&password));
        assert!(!launch.environment.contains_key("OPENCODE_PASSWORD"));
        server_stopped(root.path());
    }

    #[test]
    fn a_different_readback_never_reaches_the_tui_and_stops_the_server() {
        let (root, mut launch) = fixture();
        let mut response: Value =
            serde_json::from_slice(&fs::read(root.path().join("readback.json")).unwrap()).unwrap();
        response["data"]["model"]["id"] = json!("fallback");
        fs::write(root.path().join("readback.json"), response.to_string()).unwrap();
        assert!(prepare(&mut launch, "openai/gpt-5.2#high").is_err());
        assert_eq!(launch.argv, ["--standalone", "--auto"]);
        server_stopped(root.path());
    }

    #[test]
    fn unavailable_variant_fails_before_creating_a_session() {
        let (root, mut launch) = fixture();
        assert!(prepare(&mut launch, "openai/gpt-5.2#max").is_err());
        assert!(!root.path().join("create-args").exists());
        server_stopped(root.path());
    }

    #[test]
    fn server_output_is_bounded_after_it_becomes_ready() {
        use std::io::Write as _;
        let (root, launch) = fixture();
        let mut server = Server::start(&launch).unwrap();
        server
            .log
            .as_file_mut()
            .write_all(&vec![b'x'; usize::try_from(MAX_OUTPUT).unwrap() + 1])
            .unwrap();
        let error = api(&launch, &server, &["model.list"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("catalog server exceeded its output limit")
        );
        drop(server);
        server_stopped(root.path());
    }
}
