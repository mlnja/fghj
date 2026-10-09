//! Declared-config validation: the warnings a component earns for a `ports`
//! block or a `#RunOptions` field that cannot be carried out as written.

use std::collections::BTreeMap;

use super::build_source::{RemoteContext, parse_remote, subdir_is_contained};
use super::config::{Build, Healthcheck, default_dockerfile};
use super::normalize_repo_url;
use super::port::PortConfig;
use super::service::ServiceConfig;

use super::visit::ResolveCtx;
use super::warning::Warning;

impl<'a> ResolveCtx<'a> {
    /// Checks a `build` block and returns its remote context, if the context
    /// is a git URL. See `concepts/git-build-sources.md`.
    pub(crate) fn check_build(&mut self, id: &str, build: &Build) -> Option<RemoteContext> {
        if build.dockerfile_inline.is_some() && build.dockerfile != default_dockerfile() {
            self.warnings.push(Warning::blocking(format!(
                "'{id}' sets both `dockerfile` and `dockerfile_inline`; set one: the inline \
                 Dockerfile, or the path of one in the context"
            )));
        }
        let remote = parse_remote(&build.context)?;
        if let Some(subdir) = &remote.subdir
            && !subdir_is_contained(subdir)
        {
            self.warnings.push(Warning::blocking(format!(
                "'{id}' builds from '{subdir}' of {}, which is outside the clone; a subdir \
                 must be a relative path without `..`",
                remote.url
            )));
        }
        Some(remote)
    }

    /// Two different URLs whose clones would share one `.fghj/sources`
    /// folder: `acme/geocoder#main` and `fork/geocoder#main`. Same URL and
    /// ref is fine, and shares the clone.
    pub(crate) fn check_source_paths(&mut self) {
        let mut by_path: BTreeMap<&str, BTreeMap<String, Vec<&str>>> = BTreeMap::new();
        for node in self.nodes.values() {
            if let Some(source) = node.build.as_ref().and_then(|b| b.source.as_ref()) {
                by_path
                    .entry(&source.path)
                    .or_default()
                    .entry(normalize_repo_url(&source.url))
                    .or_default()
                    .push(&node.id);
            }
        }
        for (path, urls) in by_path {
            if urls.len() > 1 {
                let mut ids: Vec<&str> = urls.values().flatten().copied().collect();
                ids.sort_unstable();
                let urls: Vec<&str> = urls.keys().map(String::as_str).collect();
                self.warnings.push(Warning::blocking(format!(
                    "{} build from different repos ({}) that would share the clone {path}; \
                     pin different refs, or build from one of them",
                    ids.join(", "),
                    urls.join(", ")
                )));
            }
        }
    }

    /// Warns (non-fatally) when a node declares more than one `primary` port
    /// — at most one port can sit at the node's own derived domain — or a
    /// `wildcard` port that's neither `primary` nor `name`d, so there's no
    /// domain for the wildcard to apply to. Shared between services and
    /// backing dependencies, since both use the same `ports` map shape.
    /// Everything `check_http_routes` used to check (a route naming a port
    /// the service never declared) is now structurally impossible: port and
    /// role are one `ports` map entry, not two lists to keep in sync.
    pub(crate) fn check_port_config(&mut self, id: &str, ports: &BTreeMap<String, PortConfig>) {
        let primaries: Vec<&str> = ports
            .iter()
            .filter(|(_, cfg)| cfg.primary)
            .map(|(port, _)| port.as_str())
            .collect();
        if primaries.len() > 1 {
            // Blocking: exactly one port can sit at the node's own domain,
            // so a second `primary` is an instruction that cannot be carried
            // out — and which of the two wins is not something the author
            // chose.
            self.warnings.push(Warning::blocking(format!(
                "'{id}' declares more than one primary port ({}); only one can sit at its own domain",
                primaries.join(", ")
            )));
        }
        for (port, cfg) in ports {
            if cfg.wildcard && !cfg.primary && cfg.name.is_none() {
                self.warnings.push(Warning::advisory(format!(
                    "'{id}' port {port} sets wildcard but is neither primary nor named; there's no domain to wildcard"
                )));
            }
        }
    }

    /// Warns (blockingly) when `stop_signal` isn't a signal name Docker will
    /// accept. Docker also takes a bare number or an unprefixed name
    /// ("TERM"), but `#RunOptions.stop_signal` requires the `SIG` form and so
    /// does this check — the two agree exactly, and an author who writes
    /// "TERM" gets told what to write instead rather than having CUE and the
    /// daemon disagree about it.
    ///
    /// Blocking rather than advisory: Docker fails `create_container`
    /// outright on an unknown signal, so the node would not start either way
    /// — this just says so before a container is attempted, with the field
    /// name in the message instead of Docker's own bare "invalid signal".
    pub(crate) fn check_stop_signal(&mut self, id: &str, stop_signal: Option<&str>) {
        let Some(signal) = stop_signal else { return };
        let well_formed = signal.starts_with("SIG")
            && signal.len() > 3
            && signal[3..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        if !well_formed {
            self.warnings.push(Warning::blocking(format!(
                "'{id}' sets stop_signal '{signal}', which is not a signal name — write the \
                 SIG-prefixed uppercase form, e.g. SIGTERM or SIGQUIT"
            )));
        }
    }

    /// Warns (blockingly) when a `healthcheck` block is present but its
    /// `test` list is empty. Docker reads an empty `Test` as "inherit
    /// whatever the image declares", so writing `test: []` silently gets a
    /// node *no* healthcheck of its own — and since a healthcheck is what
    /// the run-wide health budget waits on, "silently no healthcheck" is the
    /// opposite of what someone typing a healthcheck block wants.
    ///
    /// `#Healthcheck.test` in CUE already says `[_, ...]` (non-empty), and
    /// the two have to agree: the Rust types are the enforcing boundary, so
    /// without this check the schema would reject a file the daemon happily
    /// accepted. See `resolver::name`'s module doc on that split.
    pub(crate) fn check_healthcheck(&mut self, id: &str, healthcheck: Option<&Healthcheck>) {
        let Some(hc) = healthcheck else { return };
        if hc.test.is_empty() {
            self.warnings.push(Warning::blocking(format!(
                "'{id}' declares a healthcheck with an empty `test` — Docker reads that \
                 as \"inherit the image's own healthcheck\", so this node would get none of \
                 its own; write the command, e.g. test: [\"CMD\", \"pg_isready\"]"
            )));
        }
    }

    pub(crate) fn check_ports(
        &mut self,
        service_id: &str,
        service: &ServiceConfig,
        ports: &BTreeMap<String, PortConfig>,
    ) {
        self.check_port_config(service_id, ports);
        self.check_stop_signal(service_id, service.stop_signal.as_deref());
        self.check_healthcheck(service_id, service.healthcheck.as_ref());
        let has_primary = ports.values().any(|cfg| cfg.primary);
        if !service.additional_hosts.is_empty() && !has_primary {
            self.warnings.push(Warning::advisory(format!(
                "'{service_id}' declares additional_hosts but no primary port; those hosts won't be routed to anything"
            )));
        }
    }
}
