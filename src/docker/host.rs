//! Which socket Docker answers on, worked out the way the user's own `docker`
//! command does. This runs in the CLI, as the user: only they have their
//! `DOCKER_HOST`, `DOCKER_CONTEXT` and `~/.docker`, and root `fghjd` has no
//! context of its own. `fghj wire` sends the answer to fghjd.
//!
//! `/var/run/docker.sock` is only the last resort. It's a symlink whichever
//! engine installed last left behind, and a stale Docker Desktop helper can
//! point it at a socket that no longer exists.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

pub const DEFAULT_SOCKET: &str = "unix:///var/run/docker.sock";

/// The `unix://…` socket this user's `docker` would talk to. Fails when it
/// isn't a unix socket, which fghjd can't dial, or nothing is listening.
pub fn users_docker_host() -> Result<String> {
    let config_dir = std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker")));
    resolve(
        std::env::var("DOCKER_HOST").ok().as_deref(),
        std::env::var("DOCKER_CONTEXT").ok().as_deref(),
        config_dir.as_deref(),
        DEFAULT_SOCKET,
    )
}

/// Docker's own precedence: `DOCKER_HOST`, then `DOCKER_CONTEXT`, then the
/// config's `currentContext`, then the default socket.
fn resolve(
    docker_host: Option<&str>,
    docker_context: Option<&str>,
    config_dir: Option<&Path>,
    default_socket: &str,
) -> Result<String> {
    let (host, from) = match docker_host.filter(|h| !h.is_empty()) {
        Some(host) => (host.to_string(), "DOCKER_HOST".to_string()),
        None => {
            let context = docker_context
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .or_else(|| config_dir.and_then(current_context));
            match context.filter(|name| name != "default") {
                Some(name) => {
                    let Some(host) = config_dir.and_then(|dir| context_host(dir, &name)) else {
                        bail!("docker context `{name}` has no endpoint fghj can read");
                    };
                    (host, format!("docker context `{name}`"))
                }
                None => (default_socket.to_string(), "the default socket".to_string()),
            }
        }
    };
    if !host.starts_with("unix://") {
        bail!("{from} is {host}, but fghjd can only reach Docker over a unix socket");
    }
    if !is_live_socket(&host) {
        bail!("nothing is listening on {host} ({from}) — is Docker running?");
    }
    Ok(host)
}

fn current_context(config_dir: &Path) -> Option<String> {
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config_dir.join("config.json")).ok()?).ok()?;
    Some(config.get("currentContext")?.as_str()?.to_string())
}

/// The built-in `default` context has no file; every other one does, under
/// the hash of its name.
fn context_host(config_dir: &Path, name: &str) -> Option<String> {
    let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
    let meta_path = config_dir
        .join("contexts/meta")
        .join(digest)
        .join("meta.json");
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(meta_path).ok()?).ok()?;
    Some(
        meta.pointer("/Endpoints/docker/Host")?
            .as_str()?
            .to_string(),
    )
}

/// Follows symlinks, so a link to a missing socket counts as missing.
pub fn is_live_socket(host: &str) -> bool {
    use std::os::unix::fs::FileTypeExt;
    host.strip_prefix("unix://")
        .and_then(|path| std::fs::metadata(path).ok())
        .is_some_and(|meta| meta.file_type().is_socket())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing listens here, whatever the machine running the tests has.
    const NOWHERE: &str = "unix:///nonexistent/fghj-test/docker.sock";

    fn context_dir(current: &str, host: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            format!(r#"{{"currentContext":"{current}"}}"#),
        )
        .unwrap();
        let meta = dir
            .path()
            .join("contexts/meta")
            .join(format!("{:x}", Sha256::digest(current.as_bytes())));
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(
            meta.join("meta.json"),
            format!(r#"{{"Name":"{current}","Endpoints":{{"docker":{{"Host":"{host}"}}}}}}"#),
        )
        .unwrap();
        dir
    }

    /// A live socket in `dir`, as `unix://…`.
    fn live_socket(dir: &Path, name: &str) -> (String, std::os::unix::net::UnixListener) {
        let path = dir.join(name);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        (format!("unix://{}", path.display()), listener)
    }

    #[test]
    fn the_active_context_names_the_socket() {
        let socks = tempfile::tempdir().unwrap();
        let (host, _l) = live_socket(socks.path(), "orb.sock");
        let dir = context_dir("orbstack", &host);
        assert_eq!(
            resolve(None, None, Some(dir.path()), NOWHERE).unwrap(),
            host
        );
    }

    #[test]
    fn docker_host_beats_docker_context_which_beats_the_config() {
        let socks = tempfile::tempdir().unwrap();
        let (env_host, _a) = live_socket(socks.path(), "env.sock");
        let (ctx_host, _b) = live_socket(socks.path(), "ctx.sock");
        let dir = context_dir("orbstack", &ctx_host);

        assert_eq!(
            resolve(Some(&env_host), Some("orbstack"), Some(dir.path()), NOWHERE).unwrap(),
            env_host
        );
        // `DOCKER_CONTEXT=default` overrides a non-default `currentContext`,
        // so only the default socket is left, and nothing listens there in a
        // test.
        let err = resolve(None, Some("default"), Some(dir.path()), NOWHERE).unwrap_err();
        assert!(err.to_string().contains("the default socket"), "{err}");
    }

    #[test]
    fn only_unix_sockets_are_usable() {
        let err = resolve(Some("tcp://10.0.0.5:2375"), None, None, NOWHERE).unwrap_err();
        assert!(
            err.to_string()
                .contains("only reach Docker over a unix socket")
        );
    }

    #[test]
    fn a_dead_socket_says_docker_isnt_running() {
        let dir = tempfile::tempdir().unwrap();
        let gone = format!("unix://{}", dir.path().join("gone.sock").display());
        let err = resolve(Some(&gone), None, None, NOWHERE).unwrap_err();
        assert!(err.to_string().contains("is Docker running"), "{err}");
    }

    #[test]
    fn a_context_with_no_metadata_is_an_error_not_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve(None, Some("vanished"), Some(dir.path()), NOWHERE).unwrap_err();
        assert!(err.to_string().contains("`vanished`"), "{err}");
    }

    #[test]
    fn a_link_to_a_missing_socket_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("docker.sock");
        std::os::unix::fs::symlink(dir.path().join("gone.sock"), &link).unwrap();
        assert!(!is_live_socket(&format!("unix://{}", link.display())));

        let (sock, _l) = live_socket(dir.path(), "live.sock");
        assert!(is_live_socket(&sock));
    }
}
