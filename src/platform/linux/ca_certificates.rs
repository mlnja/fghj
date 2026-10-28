//! The system CA store on Debian and Ubuntu: a file under
//! `/usr/local/share/ca-certificates`, folded into the bundle that OpenSSL,
//! GnuTLS, curl, Python and Go read by `update-ca-certificates`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use super::super::run;
use crate::platform::TrustStore;

/// Named for fghj, so it can't be mistaken for, or overwrite, anyone else's.
const ANCHOR: &str = "/usr/local/share/ca-certificates/fghj-local-ca.crt";
const BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

pub(super) struct CaCertificates;

impl CaCertificates {
    /// Debian's tool, which Ubuntu and its derivatives ship too.
    pub(super) fn detect() -> bool {
        [
            "/usr/sbin/update-ca-certificates",
            "/usr/bin/update-ca-certificates",
        ]
        .iter()
        .any(|p| Path::new(p).exists())
    }
}

impl TrustStore for CaCertificates {
    /// Root writes it with no prompt, so fghjd does it on every start.
    fn installed_by_user(&self) -> bool {
        false
    }

    fn install(&self, ca_cert_path: &Path) -> Result<()> {
        if is_trusted(ca_cert_path, Path::new(ANCHOR), Path::new(BUNDLE)) {
            return Ok(());
        }
        std::fs::copy(ca_cert_path, ANCHOR)
            .with_context(|| format!("failed to copy {} to {ANCHOR}", ca_cert_path.display()))?;
        run(&mut Command::new("update-ca-certificates"))?;
        println!("installed the fghj local CA into the system CA store");
        Ok(())
    }

    fn remove(&self) -> usize {
        if std::fs::remove_file(ANCHOR).is_err() {
            return 0;
        }
        // Rebuilds the bundle without it.
        let _ = run(Command::new("update-ca-certificates").arg("--fresh"));
        1
    }

    fn is_trusted(&self, ca_cert_path: &Path) -> Option<bool> {
        Some(is_trusted(
            ca_cert_path,
            Path::new(ANCHOR),
            Path::new(BUNDLE),
        ))
    }

    fn store_name(&self) -> &'static str {
        "the system CA store (/etc/ssl/certs)"
    }

    fn install_notice(&self) -> &'static str {
        "adding fghj's root CA to the system CA store"
    }

    fn untrusted_hint(&self) -> &'static str {
        "restart fghjd — it adds the CA to the system store, as root, every time it starts"
    }
}

/// Our anchor holds this CA, and the bundle programs actually read has it: a
/// copied file `update-ca-certificates` never ran over trusts nothing.
fn is_trusted(cert: &Path, anchor: &Path, bundle: &Path) -> bool {
    let Ok(ours) = std::fs::read_to_string(cert) else {
        return false;
    };
    let body = pem_body(&ours);
    if body.is_empty() {
        return false;
    }
    let anchored = std::fs::read_to_string(anchor).is_ok_and(|a| pem_body(&a) == body);
    let bundled = std::fs::read_to_string(bundle).is_ok_and(|b| squash(&b).contains(&body));
    anchored && bundled
}

/// The base64 between the markers, with no line breaks, so line endings and
/// wrapping don't matter.
fn pem_body(pem: &str) -> String {
    squash(
        &pem.lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<String>(),
    )
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CERT: &str =
        "-----BEGIN CERTIFICATE-----\nMIIBszCC\nAVmgAwIB\n-----END CERTIFICATE-----\n";
    const OTHER: &str = "-----BEGIN CERTIFICATE-----\nMIIBzzzz\n-----END CERTIFICATE-----\n";

    fn files(anchor: Option<&str>, bundle: &str) -> (tempfile::TempDir, [std::path::PathBuf; 3]) {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("ca.pem");
        let anchor_path = dir.path().join("fghj.crt");
        let bundle_path = dir.path().join("bundle.crt");
        std::fs::write(&cert, CERT).unwrap();
        if let Some(anchor) = anchor {
            std::fs::write(&anchor_path, anchor).unwrap();
        }
        std::fs::write(&bundle_path, bundle).unwrap();
        (dir, [cert, anchor_path, bundle_path])
    }

    #[test]
    fn trusted_once_it_is_both_anchored_and_in_the_bundle() {
        // Wrapped differently in the bundle, with CRLFs, as some tools write it.
        let bundle = format!(
            "{OTHER}-----BEGIN CERTIFICATE-----\r\nMIIBszCCAVmgAwIB\r\n-----END CERTIFICATE-----\r\n"
        );
        let (_dir, [cert, anchor, bundle]) = files(Some(CERT), &bundle);
        assert!(is_trusted(&cert, &anchor, &bundle));
    }

    #[test]
    fn an_anchor_the_bundle_was_never_rebuilt_from_trusts_nothing() {
        let (_dir, [cert, anchor, bundle]) = files(Some(CERT), OTHER);
        assert!(!is_trusted(&cert, &anchor, &bundle));
    }

    #[test]
    fn an_older_fghj_ca_in_our_anchor_is_not_this_one() {
        let (_dir, [cert, anchor, bundle]) = files(Some(OTHER), &format!("{CERT}{OTHER}"));
        assert!(!is_trusted(&cert, &anchor, &bundle));
    }

    #[test]
    fn no_anchor_is_not_trusted() {
        let (_dir, [cert, anchor, bundle]) = files(None, CERT);
        assert!(!is_trusted(&cert, &anchor, &bundle));
    }
}
