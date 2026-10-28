//! `fghj logs`: the tail, then with `-f` the live stream, which fghjd serves
//! as server-sent events.

use std::io::Write;

use anyhow::Result;
use http_body_util::BodyExt;

use super::client;

pub async fn print(ws: &str, node: &str, tail: usize, follow: bool, json: bool) -> Result<()> {
    let resp = client::get(&format!("/nodes/{node}/logs?workspace={ws}&tail={tail}")).await?;
    if json && !follow {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    let mut stdout = std::io::stdout();
    stdout.write_all(resp["logs"].as_str().unwrap_or_default().as_bytes())?;
    stdout.flush()?;
    if !follow {
        return Ok(());
    }
    let mut body = client::stream(&format!("/nodes/{node}/logs/stream?workspace={ws}")).await?;
    let mut buffer = String::new();
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame?.into_data() else {
            continue;
        };
        buffer.push_str(&String::from_utf8_lossy(&data));
        while let Some(end) = buffer.find("\n\n") {
            let block: String = buffer.drain(..end + 2).collect();
            match parse_event(&block) {
                Event::Data(text) => stdout.write_all(text.as_bytes())?,
                Event::Error(text) => eprintln!("fghjd: {text}"),
                Event::None => {}
            }
        }
        stdout.flush()?;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Event {
    Data(String),
    Error(String),
    /// A keep-alive comment.
    None,
}

/// One SSE block. A multi-line chunk arrives as one `data:` line per line,
/// so they're joined back with the newlines the container wrote.
fn parse_event(block: &str) -> Event {
    let mut error = false;
    let mut lines = Vec::new();
    for line in block.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            lines.push(data.strip_prefix(' ').unwrap_or(data));
        } else if line.strip_prefix("event:").map(str::trim) == Some("error") {
            error = true;
        }
    }
    if lines.is_empty() {
        return Event::None;
    }
    let text = lines.join("\n");
    if error {
        Event::Error(text)
    } else {
        Event::Data(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_comes_back_with_its_own_newlines() {
        assert_eq!(
            parse_event("data: GET /files 200\ndata: \n\n"),
            Event::Data("GET /files 200\n".to_string())
        );
    }

    #[test]
    fn an_error_event_is_told_apart() {
        assert_eq!(
            parse_event("event: error\ndata: container gone\n\n"),
            Event::Error("container gone".to_string())
        );
    }

    #[test]
    fn a_keep_alive_prints_nothing() {
        assert_eq!(parse_event(":\n\n"), Event::None);
    }
}
