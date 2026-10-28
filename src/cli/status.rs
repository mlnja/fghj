//! Text renderings of fghjd's JSON. Marks and columns, no color, so they
//! survive a pipe; `--json` is there for anything that parses.

use serde_json::Value;

/// One row per node: its state, the URL the proxy serves it on, and the raw
/// address its ports answer on. Only a node that's up has addresses.
pub fn table(env: &Value) -> String {
    let mut rows: Vec<[String; 4]> = env["containers"]
        .as_object()
        .into_iter()
        .flat_map(|c| c.iter())
        .map(|(id, c)| {
            let up = c["observed"]["status"] == "running";
            [
                id.clone(),
                state(c),
                if up { url(c) } else { String::new() },
                if up { raw(c) } else { String::new() },
            ]
        })
        .collect();
    rows.sort();
    let header = ["NODE", "STATE", "URL", "RAW"].map(String::from);
    let mut widths = [0; 4];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let line: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// What fghj is doing to it wins over what it is.
fn state(c: &Value) -> String {
    match c["pending_action"].as_str() {
        Some(action) => format!("{action}…"),
        None => c["condition"].as_str().unwrap_or("?").replace('_', " "),
    }
}

/// The primary route: the node's own domain, not a wildcard.
fn url(c: &Value) -> String {
    let routes = c["desired"]["routes"].as_array();
    let Some(route) = routes.into_iter().flatten().find(|r| r["wildcard"] != true) else {
        return String::new();
    };
    let scheme = if route["https"] == true {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{}", route["domain"].as_str().unwrap_or("?"))
}

/// The raw zone serves every published port on its own number.
fn raw(c: &Value) -> String {
    let ports: Vec<&str> = c["observed"]["ports"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.keys())
        .map(String::as_str)
        .collect();
    match (c["desired"]["raw_domain"].as_str(), ports.is_empty()) {
        (Some(domain), false) => format!("{domain}:{}", ports.join(",")),
        _ => String::new(),
    }
}

/// One line per step, as fghjd narrated it.
pub fn events(events: &[Value]) -> String {
    let mut out = String::new();
    for e in events {
        let mark = match e["status"].as_str() {
            Some("ok") => "\u{2713}",
            Some("running") => "\u{00b7}",
            Some("error" | "failed") => "\u{2717}",
            _ => "-",
        };
        out.push_str(&format!("{mark} {}", e["step"].as_str().unwrap_or("?")));
        if let Some(detail) = e["detail"].as_str() {
            out.push_str(&format!(": {detail}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env() -> Value {
        json!({ "containers": {
            "api.files-api": {
                "desired": {
                    "raw_domain": "api.files-api.files.fghj.raw.internal",
                    "routes": [
                        { "domain": "*.api.files-api.files.fghj.internal", "wildcard": true, "https": true },
                        { "domain": "api.files-api.files.fghj.internal", "wildcard": false, "https": true },
                    ],
                },
                "observed": { "status": "running", "ports": { "3000": 32780, "9229": 32781 } },
                "condition": "running",
            },
            "db.files-api": {
                "desired": { "raw_domain": "db.files-api.files.fghj.raw.internal", "routes": [] },
                "observed": { "status": "running", "ports": { "5432": 32770 } },
                "condition": "running",
                "pending_action": "stopping",
            },
            "migrate-db.files-api": {
                "desired": { "raw_domain": "migrate-db.files-api.files.fghj.raw.internal", "routes": [] },
                "observed": { "status": "exited", "ports": {} },
                "condition": "completed",
            },
        }})
    }

    #[test]
    fn a_running_node_shows_its_url_and_raw_ports() {
        let table = table(&env());
        let api = table.lines().find(|l| l.starts_with("api.")).unwrap();
        assert!(
            api.contains("https://api.files-api.files.fghj.internal"),
            "{api}"
        );
        assert!(
            api.contains("api.files-api.files.fghj.raw.internal:3000,9229"),
            "{api}"
        );
    }

    #[test]
    fn an_action_in_flight_reads_over_the_condition() {
        let table = table(&env());
        let db = table.lines().find(|l| l.starts_with("db.")).unwrap();
        assert!(db.contains("stopping…"), "{db}");
    }

    #[test]
    fn a_finished_task_has_no_addresses() {
        let table = table(&env());
        let task = table
            .lines()
            .find(|l| l.starts_with("migrate-db."))
            .unwrap();
        assert_eq!(
            task.split_whitespace().collect::<Vec<_>>(),
            ["migrate-db.files-api", "completed"]
        );
    }

    #[test]
    fn rows_are_sorted_under_a_header() {
        let table = table(&env());
        let first: Vec<&str> = table
            .lines()
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(
            first,
            [
                "NODE",
                "api.files-api",
                "db.files-api",
                "migrate-db.files-api"
            ]
        );
    }
}
