/// The `Content-Type` to serve a file under, by extension. Covers exactly
/// the file kinds the embedded UI bundle contains; anything else falls back
/// to `application/octet-stream`.
pub fn content_type_for(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_matches_extension() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type_for("app.js"),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for("styles.css"), "text/css; charset=utf-8");
        assert_eq!(content_type_for("universe.json"), "application/json");
        assert_eq!(content_type_for("logo.svg"), "image/svg+xml");
        assert_eq!(content_type_for("unknown.bin"), "application/octet-stream");
    }
}
