//! Browsers on Linux don't read the system CA store: Chrome and Chromium
//! keep an NSS database in `~/.pki/nssdb`, and every Firefox profile has its
//! own, including the snap's. fghjd adds its root to each one a workspace
//! owner has, with `certutil` (Debian's `libnss3-tools`), run as that owner.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::persistence::WorkspaceOwner;
use crate::platform::UserTrust;

/// The same name the System keychain uses on macOS.
const NICKNAME: &str = crate::web::ca::COMMON_NAME;

/// Relative to the home directory. Firefox's are profile folders under them.
const CHROMIUM_DBS: &[&str] = &[".pki/nssdb", "snap/chromium/current/.pki/nssdb"];
const FIREFOX_PROFILES: &[&str] = &[".mozilla/firefox", "snap/firefox/common/.mozilla/firefox"];

pub(super) struct NssDatabases;

impl UserTrust for NssDatabases {
    fn install(&self, user: &WorkspaceOwner, ca_cert_path: &Path) -> Result<()> {
        let dbs = databases(Path::new(&user.home));
        if dbs.is_empty() {
            return Ok(());
        }
        if !certutil_installed() {
            bail!(
                "found {} browser certificate database(s) in {}, but adding fghj's CA to them \
                 needs `certutil`: install it with `sudo apt install libnss3-tools`, then run \
                 `fghj doctor --fix`",
                dbs.len(),
                user.home
            );
        }
        let ours = pem_body(&std::fs::read_to_string(ca_cert_path)?);
        for db in dbs {
            if held(user, &db).as_deref() == Some(ours.as_str()) {
                continue;
            }
            // A CA from an earlier install would keep the nickname taken.
            delete_all(user, &db);
            let output = certutil(user, &db)
                .args(["-A", "-n", NICKNAME, "-t", "C,,", "-i"])
                .arg(ca_cert_path)
                .output()
                .context("failed to run certutil")?;
            if !output.status.success() {
                bail!(
                    "certutil couldn't add fghj's CA to {}: {}",
                    db.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            crate::daemon_log::info(format!("fghjd: trusted fghj's CA in {}", db.display()));
        }
        Ok(())
    }

    fn remove(&self, user: &WorkspaceOwner) -> usize {
        databases(Path::new(&user.home))
            .iter()
            .map(|db| delete_all(user, db))
            .sum()
    }

    /// Without `certutil` it can't tell, which reads as not trusted, so the
    /// hint says to install it.
    fn is_trusted(&self, user: &WorkspaceOwner, ca_cert_path: &Path) -> Option<bool> {
        let dbs = databases(Path::new(&user.home));
        if dbs.is_empty() {
            return None;
        }
        if !certutil_installed() {
            return Some(false);
        }
        let ours = pem_body(&std::fs::read_to_string(ca_cert_path).ok()?);
        Some(
            dbs.iter()
                .all(|db| held(user, db).as_deref() == Some(ours.as_str())),
        )
    }

    fn store_name(&self) -> &'static str {
        "the browsers' certificate databases"
    }

    fn untrusted_hint(&self) -> &'static str {
        "install `certutil` (`sudo apt install libnss3-tools`), then run `fghj doctor --fix`; \
         restart the browser afterwards"
    }
}

/// As `user`, the way fghjd runs `git clone` for them, so nothing in their
/// home becomes root's.
fn certutil(user: &WorkspaceOwner, db: &Path) -> Command {
    let mut cmd = Command::new("certutil");
    cmd.arg("-d").arg(format!("sql:{}", db.display()));
    if unsafe { libc::geteuid() } == 0 {
        user.apply_to_command(&mut cmd);
    }
    cmd
}

/// Only databases that exist: a browser that never ran has nothing to trust
/// with, and creating one would be a file nobody asked for. Legacy
/// `cert8.db` profiles are skipped; every supported Firefox migrated them.
fn databases(home: &Path) -> Vec<PathBuf> {
    let has_db = |dir: &Path| dir.join("cert9.db").is_file();
    let mut dbs: Vec<PathBuf> = CHROMIUM_DBS
        .iter()
        .map(|rel| home.join(rel))
        .filter(|dir| has_db(dir))
        .collect();
    for root in FIREFOX_PROFILES {
        let Ok(entries) = std::fs::read_dir(home.join(root)) else {
            continue;
        };
        dbs.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|dir| has_db(dir)),
        );
    }
    dbs.sort();
    dbs
}

fn certutil_installed() -> bool {
    Command::new("certutil").arg("-H").output().is_ok()
}

/// The base64 of the certificate `db` holds under fghj's nickname.
fn held(user: &WorkspaceOwner, db: &Path) -> Option<String> {
    let output = certutil(user, db)
        .args(["-L", "-a", "-n", NICKNAME])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| pem_body(&String::from_utf8_lossy(&output.stdout)))
}

/// Every certificate under fghj's nickname: `-D` removes one per call.
fn delete_all(user: &WorkspaceOwner, db: &Path) -> usize {
    let mut removed = 0;
    while removed < 32 {
        let ok = certutil(user, db)
            .args(["-D", "-n", NICKNAME])
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            break;
        }
        removed += 1;
    }
    removed
}

/// The base64 between the markers, with no line breaks. `certutil -L -a`
/// wraps differently from rcgen.
fn pem_body(pem: &str) -> String {
    pem.lines()
        .filter(|l| !l.starts_with("-----"))
        .flat_map(|l| l.chars())
        .filter(|c| !c.is_whitespace())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_chrome_and_every_firefox_profile_including_the_snaps() {
        let home = tempfile::tempdir().unwrap();
        let db = |rel: &str| {
            let dir = home.path().join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("cert9.db"), "").unwrap();
            dir
        };
        let chrome = db(".pki/nssdb");
        let firefox = db(".mozilla/firefox/abc.default-release");
        let snap = db("snap/firefox/common/.mozilla/firefox/7y4gyknq.default");
        // Not profiles: no database in them.
        std::fs::create_dir_all(
            home.path()
                .join("snap/firefox/common/.mozilla/firefox/Crash Reports"),
        )
        .unwrap();
        std::fs::create_dir_all(home.path().join("snap/chromium/current/.pki/nssdb")).unwrap();

        let mut expected = vec![chrome, firefox, snap];
        expected.sort();
        assert_eq!(databases(home.path()), expected);
    }

    #[test]
    fn a_home_with_no_browsers_has_no_databases() {
        let home = tempfile::tempdir().unwrap();
        assert!(databases(home.path()).is_empty());
    }

    #[test]
    fn pem_bodies_compare_regardless_of_wrapping() {
        let a = "-----BEGIN CERTIFICATE-----\nAAAA\nBBBB\n-----END CERTIFICATE-----\n";
        let b = "-----BEGIN CERTIFICATE-----\r\nAAAABBBB\r\n-----END CERTIFICATE-----\r\n";
        assert_eq!(pem_body(a), pem_body(b));
        assert_eq!(pem_body(a), "AAAABBBB");
    }
}
