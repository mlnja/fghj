//! Which socket fghjd dials Docker on.
//!
//! `/var/run/docker.sock` is a symlink whichever engine installed last left
//! behind, and a stale Docker Desktop helper can point it at a socket that no
//! longer exists. The user's own `docker` follows their active context
//! instead, so fghjd does too: it reads that context from the files `docker
//! context use` writes, since root has no context of its own.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const DEFAULT_SOCKET: &str = "/var/run/docker.sock";

/// `unix://…` to dial, or `None` when no socket is there. `DOCKER_HOST` isn't
/// consulted: bollard reads it itself.
pub fn find_docker_host() -> Option<String> {
    let from_context = docker_config_dir()
        .and_then(|dir| active_context_host(&dir))
        .filter(|host| socket_exists(host));
    from_context.or_else(|| {
        let default = format!("unix://{DEFAULT_SOCKET}");
        socket_exists(&default).then_some(default)
    })
}

/// The `~/.docker` of whoever is at the console, when fghjd runs as root;
/// otherwise fghjd's own.
fn docker_config_dir() -> Option<PathBuf> {
    if unsafe { libc::geteuid() } != 0 {
        return std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker"));
    }
    let owner = crate::persistence::WorkspaceOwner::of_folder(Path::new("/dev/console"))?;
    Some(PathBuf::from(owner.home).join(".docker"))
}

/// The endpoint of `currentContext`, or `None` for the built-in `default`
/// context, which has no file and means [`DEFAULT_SOCKET`].
fn active_context_host(config_dir: &Path) -> Option<String> {
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config_dir.join("config.json")).ok()?).ok()?;
    let name = config.get("currentContext")?.as_str()?;
    if name.is_empty() || name == "default" {
        return None;
    }
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
fn socket_exists(host: &str) -> bool {
    use std::os::unix::fs::FileTypeExt;
    host.strip_prefix("unix://")
        .and_then(|path| std::fs::metadata(path).ok())
        .is_some_and(|meta| meta.file_type().is_socket())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_active_context_names_the_socket() {
        let dir = context_dir("orbstack", "unix:///Users/me/.orbstack/run/docker.sock");
        assert_eq!(
            active_context_host(dir.path()).as_deref(),
            Some("unix:///Users/me/.orbstack/run/docker.sock")
        );
    }

    #[test]
    fn the_default_context_has_no_file() {
        let dir = context_dir("default", "unix:///ignored");
        assert_eq!(active_context_host(dir.path()), None);
    }

    #[test]
    fn a_link_to_a_missing_socket_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("docker.sock");
        std::os::unix::fs::symlink(dir.path().join("gone.sock"), &link).unwrap();
        assert!(!socket_exists(&format!("unix://{}", link.display())));

        let sock = dir.path().join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(socket_exists(&format!("unix://{}", sock.display())));
    }
}
