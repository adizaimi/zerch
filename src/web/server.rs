//! The tiny_http server loop and request routing. Thin: it parses the request path,
//! calls a handler, and writes the response. A small fixed worker pool lets a few
//! requests (e.g. parallel row fetches from one tab) proceed concurrently.

use std::collections::HashMap;
use std::sync::Arc;

use tiny_http::{Header, Request, Response, Server};

use super::assets;
use super::handlers;
use super::State;

/// Spawn `workers` threads, each blocking on `incoming_requests`, and join them.
/// `run` calls this last, so it blocks the process until Ctrl+C tears the threads down.
pub fn serve(server: Arc<Server>, state: Arc<State>, workers: usize) {
    let mut handles = Vec::new();
    for _ in 0..workers.max(1) {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || {
            for req in server.incoming_requests() {
                handle(&state, req);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

fn handle(state: &State, req: Request) {
    // req.url() includes the query string; route on the path, parse the query lazily.
    let url = req.url().to_string();
    let mut parts = url.splitn(2, '?');
    let path = parts.next().unwrap_or("/");
    let query = parts.next().unwrap_or("");
    match path {
        "/" => respond(req, 200, "text/html; charset=utf-8", index_page(state, query)),
        "/app.css" => respond(req, 200, "text/css; charset=utf-8", assets::APP_CSS.to_string()),
        "/app.js" => respond(
            req,
            200,
            "text/javascript; charset=utf-8",
            assets::APP_JS.to_string(),
        ),
        "/rows" => {
            let (status, body) = handlers::rows(state, &parse_query(query));
            respond(req, status, "application/json; charset=utf-8", body);
        }
        "/matches" => {
            let (status, body) = handlers::matches(state, &parse_query(query));
            respond(req, status, "application/json; charset=utf-8", body);
        }
        "/goto" => {
            let (status, body) = handlers::goto(state, &parse_query(query));
            respond(req, status, "application/json; charset=utf-8", body);
        }
        _ => respond(req, 404, "text/plain; charset=utf-8", "not found\n".to_string()),
    }
}

/// Decode a URL query string into a map (form-urlencoded, last value wins).
fn parse_query(query: &str) -> HashMap<String, String> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// Serve `index.html` with the bootstrap object injected as JSON. The seed view is
/// the request's own query string when present (so a shared URL restores that exact
/// view), else the token resolved at startup (`--view` / saved session). `</` is
/// escaped to `<\/` so a file name or log content containing `</script>` can't break
/// out of the `<script type="application/json">` block.
fn index_page(state: &State, query: &str) -> String {
    let view = if query.is_empty() {
        state.initial_view.clone()
    } else {
        query.to_string()
    };
    let boot = serde_json::json!({
        "file": state.file_name,
        "total": state.index.total_lines(),
        "maxlen": state.index.max_line_length(),
        "view": view,
    })
    .to_string()
    .replace("</", "<\\/");
    assets::INDEX_HTML.replace("__BOOTSTRAP__", &boot)
}

fn respond(req: Request, status: u16, content_type: &str, body: String) {
    let header =
        Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).expect("valid header");
    let resp = Response::from_string(body)
        .with_status_code(status)
        .with_header(header);
    let _ = req.respond(resp);
}
