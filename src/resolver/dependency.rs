//! `depends_on` — Compose's syntax, with one fghj addition: a target can be
//! another repo's flow (`billing/pricing`) as well as a service in this one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Compose's `depends_on`: a bare list of names, or a map of name to the
/// long form.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum DependsOn {
    List(Vec<String>),
    Map(BTreeMap<String, Option<DependsOnEntry>>),
}

impl Default for DependsOn {
    fn default() -> Self {
        DependsOn::List(Vec::new())
    }
}

impl DependsOn {
    /// Every entry with the long form filled in, in declaration order (a
    /// list) or name order (a map).
    pub(crate) fn entries(&self) -> Vec<(String, DependsOnEntry)> {
        match self {
            DependsOn::List(names) => names
                .iter()
                .map(|n| (n.clone(), DependsOnEntry::default()))
                .collect(),
            DependsOn::Map(map) => map
                .iter()
                .map(|(n, e)| (n.clone(), e.clone().unwrap_or_default()))
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DependsOnEntry {
    #[serde(default)]
    pub(crate) condition: Condition,
    /// `false` means "uses it, but can start without it": the target is
    /// only started when a flow asks for it, and ordered before this
    /// service when both are in a run.
    #[serde(default = "default_required")]
    pub(crate) required: bool,
}

impl Default for DependsOnEntry {
    fn default() -> Self {
        DependsOnEntry {
            condition: Condition::default(),
            required: true,
        }
    }
}

fn default_required() -> bool {
    true
}

/// Compose's three `depends_on` conditions.
///
/// A required edge waits for its target to be ready — healthy if it
/// declares a healthcheck, exited 0 if it's a task, running otherwise — so
/// the condition is checked against the target rather than changing how
/// long anything waits. `service_completed_successfully` is also one of the
/// two things that make the target a task. A `required: false` edge waits
/// on nothing; see `concepts/dependency-kinds.md`.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    #[default]
    ServiceStarted,
    ServiceHealthy,
    ServiceCompletedSuccessfully,
}

impl Condition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Condition::ServiceStarted => "service_started",
            Condition::ServiceHealthy => "service_healthy",
            Condition::ServiceCompletedSuccessfully => "service_completed_successfully",
        }
    }
}

/// What a `depends_on` key or a flow entry points at, before it is checked
/// against anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    /// No `/`: a service or flow in this repo, or an included repo as a
    /// whole — which one is decided against the file's own names.
    Bare(&'a str),
    /// `alias/name`: a flow (or, in a hostname, a service) of an included
    /// repo.
    Qualified { alias: &'a str, name: &'a str },
}

impl<'a> Target<'a> {
    pub(crate) fn parse(s: &'a str) -> Target<'a> {
        match s.split_once('/') {
            Some((alias, name)) => Target::Qualified { alias, name },
            None => Target::Bare(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_and_a_map_mean_the_same_thing() {
        let list: DependsOn = serde_yaml::from_str("[db, cache]").unwrap();
        let map: DependsOn = serde_yaml::from_str("{db: {}, cache: null}").unwrap();
        for deps in [list, map] {
            let entries = deps.entries();
            assert_eq!(entries.len(), 2);
            for (_, e) in entries {
                assert!(e.required);
                assert_eq!(e.condition, Condition::ServiceStarted);
            }
        }
    }

    #[test]
    fn the_long_form_reads_condition_and_required() {
        let deps: DependsOn =
            serde_yaml::from_str("{db: {condition: service_healthy}, search: {required: false}}")
                .unwrap();
        let entries: BTreeMap<_, _> = deps.entries().into_iter().collect();
        assert_eq!(entries["db"].condition, Condition::ServiceHealthy);
        assert!(entries["db"].required);
        assert!(!entries["search"].required);
    }

    #[test]
    fn an_unknown_condition_is_refused() {
        assert!(serde_yaml::from_str::<DependsOnEntry>("{condition: service_ready}").is_err());
    }

    #[test]
    fn targets_split_on_the_first_slash() {
        assert_eq!(Target::parse("db"), Target::Bare("db"));
        assert_eq!(
            Target::parse("billing/pricing"),
            Target::Qualified {
                alias: "billing",
                name: "pricing"
            }
        );
    }
}
