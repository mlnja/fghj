//! Builds with the workspace owner's own `docker buildx build`. That is the
//! only way to build where their `docker build` would: the same builder, the
//! same cache, and their registry credentials (credential helpers, the
//! keychain), none of which root `fghjd` has. See `concepts/builder-parity.md`.
//!
//! The context is fghj's own tar, sent on stdin, so `.dockerignore`, a git
//! source's `.git` and an inline Dockerfile are handled exactly as on the
//! bollard path.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bollard::Docker;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};

use super::BuildOpts;
use crate::persistence::WorkspaceOwner;

/// How much of the build's output a failure carries.
const TAIL_LINES: usize = 40;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// A `docker` CLI with the buildx plugin, and who to run it as.
#[derive(Debug, Clone)]
pub struct BuildxCli {
    docker: PathBuf,
    home: PathBuf,
    /// `Some((uid, gid))` when `fghjd` is root: the CLI runs as this user, in
    /// their login session, since their credential store only answers there.
    as_user: Option<(u32, u32)>,
}

impl BuildxCli {
    /// `None` when there is no `docker` binary or no buildx plugin, or no
    /// owner to run it as; the caller then builds through bollard.
    pub async fn find(owner: Option<&WorkspaceOwner>) -> Option<Self> {
        let is_root = unsafe { libc::geteuid() } == 0;
        let (home, as_user) = match (is_root, owner) {
            (true, Some(owner)) => (PathBuf::from(&owner.home), Some((owner.uid, owner.gid))),
            // Root's own `~/.docker` holds none of the user's builders or logins.
            (true, None) => return None,
            (false, _) => (PathBuf::from(std::env::var_os("HOME")?), None),
        };
        if let Some((uid, gid)) = as_user
            && crate::platform::current()
                .session
                .as_user_in_session(uid, gid, &[], &[])
                .is_none()
        {
            crate::daemon_log::info(
                "fghjd: no way to run the docker CLI as the workspace owner on this OS; \
                 building through the Docker API instead",
            );
            return None;
        }
        let Some(docker) = docker_candidates(&home)
            .into_iter()
            .find(|p| is_executable(p))
        else {
            crate::daemon_log::info(
                "fghjd: no docker CLI found; building through the Docker API instead",
            );
            return None;
        };
        let cli = Self {
            docker,
            home,
            as_user,
        };

        let mut probe = cli.command(&[OsString::from("buildx"), OsString::from("version")], None);
        probe
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(PROBE_TIMEOUT, probe.output()).await {
            Ok(Ok(out)) if out.status.success() => Some(cli),
            Ok(Ok(out)) => {
                crate::daemon_log::info(format!(
                    "fghjd: `{} buildx version` failed ({}): {}; building through the Docker API instead",
                    cli.docker.display(),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                None
            }
            Ok(Err(e)) => {
                crate::daemon_log::info(format!(
                    "fghjd: could not run {}: {e}; building through the Docker API instead",
                    cli.docker.display()
                ));
                None
            }
            Err(_) => {
                crate::daemon_log::info(format!(
                    "fghjd: `{} buildx version` did not answer in {}s; building through the Docker API instead",
                    cli.docker.display(),
                    PROBE_TIMEOUT.as_secs()
                ));
                None
            }
        }
    }

    /// Runs `docker buildx build` on `context` (fghj's tar of the build
    /// context) and checks the image reached the Docker `fghjd` talks to.
    pub async fn build(
        &self,
        docker: &Docker,
        opts: &BuildOpts<'_>,
        context: Vec<u8>,
    ) -> Result<()> {
        let mut cmd = self.command(&build_args(opts), opts.ssh_auth_sock);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Its own group, so calling the start off can stop the whole chain.
            .process_group(0);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to run {}", self.docker.display()))?;
        let guard = child.id().map(StopOnDrop);

        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));
        let stdout = tokio::spawn(collect_tail(child.stdout.take(), tail.clone()));
        let stderr = tokio::spawn(collect_tail(child.stderr.take(), tail.clone()));
        let mut stdin = child.stdin.take().context("stdin of docker buildx build")?;
        let fed = async move {
            stdin.write_all(&context).await?;
            stdin.shutdown().await
        };
        // A build that fails early stops reading; its exit status says why.
        let (fed, status) = tokio::join!(fed, child.wait());
        let status = status.context("failed waiting for docker buildx build")?;
        let _ = tokio::join!(stdout, stderr);
        if let Some(guard) = guard {
            std::mem::forget(guard);
        }

        let tail: Vec<String> = tail
            .lock()
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default();
        if !status.success() {
            bail!(
                "build of {} failed (docker buildx build: {status})\n\n{}",
                opts.tag,
                tail.join("\n")
            );
        }
        fed.context("failed to send the build context to docker buildx build")?;

        if docker.inspect_image(opts.tag).await.is_err() {
            bail!(
                "docker buildx built {}, but the Docker fghjd uses doesn't have it: your \
                 `docker` CLI and fghjd are talking to different engines. Run `fghj wire` \
                 from the shell you build in",
                opts.tag
            );
        }
        Ok(())
    }

    fn command(&self, args: &[OsString], ssh_auth_sock: Option<&str>) -> tokio::process::Command {
        let env = cli_env(&self.docker, &self.home, ssh_auth_sock);
        let mut argv = vec![self.docker.as_os_str().to_owned()];
        argv.extend(args.iter().cloned());
        let line = self.as_user.and_then(|(uid, gid)| {
            crate::platform::current()
                .session
                .as_user_in_session(uid, gid, &env, &argv)
        });
        match line {
            Some(line) => {
                let mut cmd = tokio::process::Command::new(&line[0]);
                cmd.args(&line[1..]);
                cmd
            }
            None => {
                let mut cmd = tokio::process::Command::new(&argv[0]);
                cmd.args(&argv[1..]).env_clear().envs(env);
                cmd
            }
        }
    }
}

/// Sends the build's process group SIGTERM, which the CLI turns into a
/// cancelled build; SIGKILL would leave the build running in BuildKit.
struct StopOnDrop(u32);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        unsafe {
            libc::killpg(self.0 as libc::pid_t, libc::SIGTERM);
        }
    }
}

async fn collect_tail(stream: Option<impl AsyncRead + Unpin>, tail: Arc<Mutex<VecDeque<String>>>) {
    let Some(stream) = stream else { return };
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(mut tail) = tail.lock() {
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
}

fn cli_env(docker: &Path, home: &Path, ssh_auth_sock: Option<&str>) -> Vec<(String, OsString)> {
    let mut env = vec![
        (String::from("HOME"), home.as_os_str().to_owned()),
        (String::from("PATH"), search_path(docker, home)),
        // Hints and plugin hooks (`docker scout` on every build) are for a
        // person at a terminal.
        (String::from("DOCKER_CLI_HINTS"), OsString::from("false")),
        (String::from("DOCKER_CLI_HOOKS"), OsString::from("false")),
    ];
    if let Some(sock) = ssh_auth_sock {
        env.push((String::from("SSH_AUTH_SOCK"), OsString::from(sock)));
    }
    env
}

/// Where the CLI finds its credential helpers (`docker-credential-*`): next to
/// itself, where OrbStack and Docker Desktop put them, then the system's.
fn search_path(docker: &Path, home: &Path) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = docker.parent() {
        dirs.push(dir.to_path_buf());
    }
    dirs.push(home.join(".orbstack/bin"));
    dirs.push(home.join(".docker/bin"));
    dirs.push(PathBuf::from(
        "/Applications/Docker.app/Contents/Resources/bin",
    ));
    for dir in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        dirs.push(PathBuf::from(dir));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_default()
}

/// `fghjd` runs under launchd with a bare `PATH`, so the usual places by hand.
fn docker_candidates(home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin/docker"),
        PathBuf::from("/usr/local/bin/docker"),
        PathBuf::from("/usr/bin/docker"),
        PathBuf::from("/snap/bin/docker"),
        home.join(".orbstack/bin/docker"),
        home.join(".docker/bin/docker"),
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin/docker"),
        PathBuf::from("/Applications/OrbStack.app/Contents/MacOS/xbin/docker"),
    ]
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `-` reads the context, a tar, from stdin; `-f` names a file inside it.
fn build_args(opts: &BuildOpts<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "buildx",
        "build",
        "--progress=plain",
        "--load",
        "-t",
        opts.tag,
        "-f",
        opts.dockerfile,
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(target) = opts.target {
        args.extend([OsString::from("--target"), OsString::from(target)]);
    }
    if let Some(platform) = opts.platform {
        args.extend([OsString::from("--platform"), OsString::from(platform)]);
    }
    for (key, value) in opts.args {
        args.push(OsString::from("--build-arg"));
        args.push(OsString::from(format!("{key}={value}")));
    }
    for (id, path) in opts.secrets {
        args.push(OsString::from("--secret"));
        args.push(OsString::from(format!(
            "{},{}",
            csv_field(&format!("id={id}")),
            csv_field(&format!("src={}", path.display()))
        )));
    }
    if let Some(sock) = opts.ssh_auth_sock {
        args.push(OsString::from("--ssh"));
        args.push(OsString::from(format!("default={sock}")));
    }
    args.push(OsString::from("-"));
    args
}

/// buildx reads `--secret` as one CSV record, so a comma in a path needs quoting.
fn csv_field(field: &str) -> String {
    if field.contains([',', '"']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn opts<'a>(
        args: &'a BTreeMap<String, String>,
        secrets: &'a [(String, PathBuf)],
    ) -> BuildOpts<'a> {
        BuildOpts {
            context_dir: Path::new("/src/app"),
            dockerfile: ".fghj.Dockerfile",
            dockerfile_inline: None,
            skip_git_dir: false,
            tag: "fghj/app:main",
            platform: Some("linux/amd64"),
            args,
            target: Some("runtime"),
            secrets,
            ssh_auth_sock: Some("/tmp/agent.sock"),
            builder: None,
            cli: None,
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn every_build_input_becomes_its_buildx_flag_and_the_context_comes_on_stdin() {
        let mut build_args_map = BTreeMap::new();
        build_args_map.insert(String::from("GREETING"), String::from("a b=c"));
        let secrets = [
            (String::from("npm"), PathBuf::from("/home/me/.npmrc")),
            (String::from("odd"), PathBuf::from("/home/me/a,b")),
        ];
        assert_eq!(
            strings(&build_args(&opts(&build_args_map, &secrets))),
            [
                "buildx",
                "build",
                "--progress=plain",
                "--load",
                "-t",
                "fghj/app:main",
                "-f",
                ".fghj.Dockerfile",
                "--target",
                "runtime",
                "--platform",
                "linux/amd64",
                "--build-arg",
                "GREETING=a b=c",
                "--secret",
                "id=npm,src=/home/me/.npmrc",
                "--secret",
                "id=odd,\"src=/home/me/a,b\"",
                "--ssh",
                "default=/tmp/agent.sock",
                "-",
            ]
        );
    }

    #[test]
    fn not_as_root_the_cli_runs_directly_with_only_its_own_env() {
        let cli = BuildxCli {
            docker: PathBuf::from("/usr/local/bin/docker"),
            home: PathBuf::from("/Users/me"),
            as_user: None,
        };
        let cmd = cli.command(&[OsString::from("buildx")], Some("/tmp/agent.sock"));
        let cmd = cmd.as_std();
        assert_eq!(cmd.get_program(), "/usr/local/bin/docker");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["buildx"]);
        let env: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(env["HOME"], "/Users/me");
        assert_eq!(env["SSH_AUTH_SOCK"], "/tmp/agent.sock");
        assert_eq!(env["DOCKER_CLI_HOOKS"], "false");
    }

    #[test]
    fn the_search_path_starts_next_to_the_cli_and_names_each_dir_once() {
        let path = search_path(Path::new("/usr/local/bin/docker"), Path::new("/Users/me"));
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[0], Path::new("/usr/local/bin"));
        assert_eq!(
            dirs.iter()
                .filter(|d| *d == Path::new("/usr/local/bin"))
                .count(),
            1
        );
        assert!(dirs.contains(&PathBuf::from("/Users/me/.orbstack/bin")));
    }
}
