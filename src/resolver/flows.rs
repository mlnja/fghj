//! Flows — a repo's public start lists. See `concepts/flows-v2.md`.
//!
//! An entry is a service or flow of the same repo, `alias/flow` (another
//! repo's flow), or `alias` (all of another repo, and everything it
//! includes). Another repo's *services* are never named: they're its
//! internals, and its flows are the contract.
//!
//! A flow's id is `{local_path}/{flow}`. What a run of it starts is its
//! members plus everything they can't start without — the closure over
//! required "depends-on" edges ([`super::graph::required_closure`]).

use std::collections::BTreeSet;

use super::dependency::Target;
use super::graph::required_closure;
use super::visit::{ResolveCtx, node_id};
use super::warning::Warning;

impl<'a> ResolveCtx<'a> {
    /// The node ids a cross-repo `depends_on` target stands for: the
    /// included repo's own nodes (`alias`), or one of its flows
    /// (`alias/flow`). `Err` says why the target names nothing, to follow
    /// the dependent's id.
    ///
    /// Unlike `alias` in a flow, waiting on a repo doesn't reach through
    /// what *it* includes: its services already wait on what they need from
    /// there, and an include pointing back at the dependent would make it
    /// wait on itself.
    pub(crate) fn foreign_members(
        &self,
        local_path: &str,
        target: Target<'_>,
    ) -> Result<Vec<String>, String> {
        let mut out = BTreeSet::new();
        match target {
            Target::Bare(alias) => {
                let repo = self.alias_target(local_path, alias).unwrap_or(alias);
                let mut seen = BTreeSet::new();
                if let Some(includes) = self.includes.get(repo) {
                    // Marked seen, so only `repo` itself is walked.
                    seen.extend(includes.values().cloned());
                }
                seen.remove(repo);
                self.repo_members(repo, &mut seen, &mut out);
            }
            Target::Qualified { alias, name } => {
                let Some(repo) = self.alias_target(local_path, alias) else {
                    return Err(format!(
                        "names '{alias}/{name}', but '{local_path}' has no include named '{alias}'"
                    ));
                };
                self.check_foreign_flow(alias, repo, name)?;
                self.expand_flow(
                    repo,
                    name,
                    1,
                    &mut BTreeSet::new(),
                    &mut out,
                    &mut Vec::new(),
                );
            }
        }
        Ok(out.into_iter().collect())
    }

    /// `alias/name` must be a flow of the repo `alias` points at. A repo not
    /// pulled yet can't be checked, so it passes.
    fn check_foreign_flow(&self, alias: &str, repo: &str, name: &str) -> Result<(), String> {
        let Some(component) = self.scanned.get(repo) else {
            return Ok(());
        };
        if component.flows.contains_key(name) {
            return Ok(());
        }
        let available = if component.flows.is_empty() {
            format!("it publishes none, so name '{alias}' to start all of it")
        } else {
            let names: Vec<String> = component
                .flows
                .keys()
                .map(|f| format!("{alias}/{f}"))
                .collect();
            format!("use one of its flows: {}", names.join(", "))
        };
        if component.services.contains_key(name) {
            Err(format!(
                "names '{alias}/{name}', which is a service — {alias}'s services are internal; \
                 {available}"
            ))
        } else {
            Err(format!(
                "names '{alias}/{name}', but '{repo}' has no flow named '{name}'; {available}"
            ))
        }
    }

    /// Every node in `repo`, and in every repo it includes, transitively. A
    /// repo not pulled yet is its stub node.
    fn repo_members(&self, repo: &str, seen: &mut BTreeSet<String>, out: &mut BTreeSet<String>) {
        if !seen.insert(repo.to_string()) {
            return;
        }
        if !self.is_scanned(repo) {
            if self.nodes.contains_key(repo) {
                out.insert(repo.to_string());
            }
            return;
        }
        out.extend(
            self.nodes
                .values()
                .filter(|n| n.downloaded && n.local_path.as_deref() == Some(repo))
                .map(|n| n.id.clone()),
        );
        if let Some(includes) = self.includes.get(repo) {
            for target in includes.values() {
                self.repo_members(target, seen, out);
            }
        }
    }

    /// Adds the members of `repo`'s flow `flow` to `out`. Each `(repo, flow)`
    /// is expanded once, so flows naming each other — even across repos —
    /// terminate. A bad entry is reported only at `depth` 0, by the flow
    /// that contains it, so it's named once however many flows reach it.
    fn expand_flow(
        &self,
        repo: &str,
        flow: &str,
        depth: usize,
        seen: &mut BTreeSet<(String, String)>,
        out: &mut BTreeSet<String>,
        errors: &mut Vec<String>,
    ) {
        if !seen.insert((repo.to_string(), flow.to_string())) {
            return;
        }
        let Some(component) = self.scanned.get(repo) else {
            if self.nodes.contains_key(repo) {
                out.insert(repo.to_string());
            }
            return;
        };
        let Some(entries) = component.flows.get(flow) else {
            return;
        };
        for entry in entries {
            match Target::parse(entry) {
                Target::Bare(name) if component.services.contains_key(name) => {
                    out.insert(node_id(name, repo));
                }
                Target::Bare(name) if component.flows.contains_key(name) => {
                    self.expand_flow(repo, name, depth + 1, seen, out, errors);
                }
                Target::Bare(alias) if self.has_alias(repo, alias) => {
                    let target = self.alias_target(repo, alias).unwrap_or(alias).to_string();
                    self.repo_members(&target, &mut BTreeSet::new(), out);
                }
                Target::Qualified { alias, name } if self.has_alias(repo, alias) => {
                    let target = self.alias_target(repo, alias).unwrap_or(alias).to_string();
                    match self.check_foreign_flow(alias, &target, name) {
                        Ok(()) => self.expand_flow(&target, name, depth + 1, seen, out, errors),
                        Err(e) if depth == 0 => errors.push(e),
                        Err(_) => {}
                    }
                }
                _ if depth == 0 => errors.push(format!(
                    "names '{entry}', but '{repo}' has no service, flow or include by that name"
                )),
                _ => {}
            }
        }
    }

    /// Phase 6, over the finished node and edge set: every flow's id, with
    /// `Node.flows` and `Edge.flows` filled in from its start set.
    pub(crate) fn resolve_flows(&mut self) -> Vec<String> {
        let mut flow_ids = Vec::new();
        let mut memberships: Vec<(String, BTreeSet<String>)> = Vec::new();
        for (repo, component) in self.scanned {
            for flow in component.flows.keys() {
                let id = format!("{repo}/{flow}");
                let mut members = BTreeSet::new();
                let mut errors = Vec::new();
                self.expand_flow(
                    repo,
                    flow,
                    0,
                    &mut BTreeSet::new(),
                    &mut members,
                    &mut errors,
                );
                for e in errors {
                    self.warnings
                        .push(Warning::blocking(format!("flow '{id}' {e}")));
                }
                let start = required_closure(&self.edges, members);
                flow_ids.push(id.clone());
                memberships.push((id, start));
            }
        }
        for (id, start) in &memberships {
            for node in start {
                if let Some(node) = self.nodes.get_mut(node) {
                    node.flows.push(id.clone());
                }
            }
            for edge in &mut self.edges {
                if start.contains(&edge.from) && start.contains(&edge.to) {
                    edge.flows.push(id.clone());
                }
            }
        }
        flow_ids
    }
}
