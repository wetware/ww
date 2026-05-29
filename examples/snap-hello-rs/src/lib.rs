//! snap-hello-rs — Farcaster Snap POC hosted on wetware.
//!
//! v1.5 surface: text + button. GET renders an anonymous "Hello,
//! @stranger" greeting plus a "Ping me" button. Pressing the button
//! POSTs back to the same URL with a JFS-signed payload (POST is
//! REQUIRED to be JFS-signed per spec); the response renders
//! "Hello, FID #N — pinged at <utc-timestamp>" plus another button
//! to ping again. The button forces every clicking user's client to
//! send `X-Snap-Payload`, which is how we exercise viewer-aware
//! rendering even on Farcaster clients whose render-time GETs are
//! anonymous server-side fetches.
//!
//! Content negotiation:
//!   - `Accept: application/vnd.farcaster.snap+json` → snap-JSON
//!   - anything else                                  → HTML + Link rel=alternate
//!
//! Spec: https://docs.farcaster.xyz/snap/spec-overview
//!       https://docs.farcaster.xyz/snap/http-headers
//!       https://docs.farcaster.xyz/snap/auth
//!       https://docs.farcaster.xyz/snap/buttons
//!
//! Stateless. Fresh cell per request. No graft caps used.
//!
//! IMPORTANT — FID trust model: `X_SNAP_FID_CLAIMED` is demo metadata
//! supplied by the Farcaster snap example flow.

use std::time::{SystemTime, UNIX_EPOCH};

use wagi_guest as wagi;
use wasip2::exports::cli::run::Guest;

const SNAP_TYPE: &str = "application/vnd.farcaster.snap+json";

/// Render the viewer's greeting from snap-example env vars.
/// Returns `"FID #<n>"` when present, else `"@stranger"`.
fn viewer_greeting() -> String {
    match std::env::var("X_SNAP_FID_CLAIMED") {
        Ok(fid) if !fid.is_empty() => format!("FID #{fid}"),
        _ => "@stranger".to_string(),
    }
}

/// Build the absolute URL that snap submit-action buttons should POST
/// to. Per spec, `params.target` is required and must be an HTTPS URL.
/// Reads `HTTP_HOST` (the Host header forwarded by the listener) +
/// `PATH_INFO` and assumes HTTPS (production deploys behind a TLS
/// terminator like Traefik). For local-dev `http://` setups the cell
/// would need a plumbing tweak, out of scope for this demo.
fn compute_target_url() -> String {
    let host = wagi::header("Host").unwrap_or_else(|| "master.wetware.run".to_string());
    let path = wagi::path();
    let path = if path.is_empty() { "/snaps/hello" } else { &path };
    format!("https://{host}{path}")
}

/// Current UTC time formatted compactly for the ping response. We avoid
/// a chrono dep in the cell — `SystemTime` from std is sufficient and
/// works under wasip2 via `wasi:clocks/wall-clock`.
fn now_utc_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Don't pull in chrono just to format — `<unix>` + a "UTC" suffix
    // is plenty for a demo; avoids a 200KB wasm bloat.
    format!("{secs} UTC (unix)")
}

/// Build the snap-JSON UI tree. `text_content` is the greeting line,
/// `target_url` is where the button POSTs to. The shape is a vertical
/// stack containing the greeting text + a primary button labeled
/// "Ping me".
fn snap_response_json(text_content: &str, target_url: &str) -> String {
    serde_json::json!({
        "version": "2.0",
        "ui": {
            "root": "root",
            "elements": {
                "root": {
                    "type": "stack",
                    "props": { "direction": "vertical", "gap": "sm" },
                    "children": ["greeting", "ping_button"]
                },
                "greeting": {
                    "type": "text",
                    "props": { "content": text_content }
                },
                "ping_button": {
                    "type": "button",
                    "props": { "label": "Ping me", "variant": "primary" },
                    "on": {
                        "press": {
                            "action": "submit",
                            "params": { "target": target_url }
                        }
                    }
                }
            }
        }
    })
    .to_string()
}

/// HTML fallback for non-Farcaster visitors. Same content as before
/// (no button — pressing requires snap-JSON rendering).
fn html_body(greeting: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>Hello from a wetware snap</title>
  <link rel="alternate" type="application/vnd.farcaster.snap+json" href="">
  <meta property="og:title" content="Hello from a wetware snap">
</head>
<body>
  <h1>Hello, {greeting}</h1>
  <p>This is a Farcaster Snap hosted on wetware.
     Open this URL in a Farcaster client to render it (with a
     "Ping me" button).</p>
</body>
</html>"#
    )
}

/// True when the request's `Accept` header mentions the snap media type.
fn wants_snap(accept: &str) -> bool {
    accept.contains(SNAP_TYPE)
}

/// Greeting text for the snap-JSON path.
///
/// - Anonymous GET: `"Hello, @stranger"` (no JFS context).
/// - Viewer-aware GET: `"Hello, FID #N"` (verified payload reached us).
/// - POST (any): `"Hello, FID #N — pinged at <ts>"` (POST requires JFS
///   per spec; the timestamp gives the response visible dynamism).
fn snap_text(method: &str) -> String {
    let greeting = viewer_greeting();
    if method == "POST" {
        format!("Hello, {greeting} — pinged at {}", now_utc_string())
    } else {
        format!("Hello, {greeting}")
    }
}

struct SnapCell;

impl Guest for SnapCell {
    fn run() -> Result<(), ()> {
        let accept = wagi::header("Accept").unwrap_or_default();
        let method = wagi::method();
        let is_post = method == "POST";

        // `respond_bytes` flushes explicitly; plain `respond` uses
        // `print!` and can lose buffered bytes on cell teardown
        // (the body sits in stdout while only the headers ship).
        // Same fix the std/status cell uses (std/status/src/lib.rs:151-153).
        if wants_snap(&accept) || is_post {
            let body = snap_response_json(&snap_text(&method), &compute_target_url());
            wagi::respond_bytes(
                200,
                &[
                    ("Content-Type", SNAP_TYPE),
                    ("Vary", "Accept"),
                    // POST responses are per-viewer (carry the verified
                    // FID + a fresh timestamp). Don't cache them.
                    // GET-anonymous can still be cached safely.
                    if is_post {
                        ("Cache-Control", "private, no-store")
                    } else {
                        ("Cache-Control", "public, max-age=300")
                    },
                    ("Access-Control-Allow-Origin", "*"),
                ],
                body.as_bytes(),
            );
        } else {
            // `Link: <>; rel="alternate"; type="..."` — empty `<>` is
            // an RFC 3986 same-document reference; clients re-fetch
            // the current URL with `Accept` set. Avoids hardcoding an
            // absolute URL into the cell.
            let body = html_body(&viewer_greeting());
            wagi::respond_bytes(
                200,
                &[
                    ("Content-Type", "text/html; charset=utf-8"),
                    ("Vary", "Accept"),
                    ("Cache-Control", "public, max-age=300"),
                    ("Access-Control-Allow-Origin", "*"),
                    (
                        "Link",
                        "<>; rel=\"alternate\"; type=\"application/vnd.farcaster.snap+json\"",
                    ),
                ],
                body.as_bytes(),
            );
        }

        Ok(())
    }
}

wasip2::cli::command::export!(SnapCell);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wants_snap_exact_match() {
        assert!(wants_snap("application/vnd.farcaster.snap+json"));
    }

    #[test]
    fn wants_snap_in_accept_list() {
        assert!(wants_snap(
            "text/html, application/vnd.farcaster.snap+json, */*"
        ));
    }

    #[test]
    fn wants_snap_html_only_returns_false() {
        assert!(!wants_snap("text/html"));
    }

    #[test]
    fn wants_snap_empty_returns_false() {
        assert!(!wants_snap(""));
    }

    #[test]
    fn snap_response_required_top_level_fields() {
        let body =
            snap_response_json("Hello, @stranger", "https://master.wetware.run/snaps/hello");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["version"], "2.0");
        assert_eq!(v["ui"]["root"], "root");
    }

    #[test]
    fn snap_response_stack_root_with_two_children() {
        let body =
            snap_response_json("Hello, @stranger", "https://master.wetware.run/snaps/hello");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ui"]["elements"]["root"]["type"], "stack");
        let children = v["ui"]["elements"]["root"]["children"]
            .as_array()
            .expect("root.children should be an array");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0], "greeting");
        assert_eq!(children[1], "ping_button");
    }

    #[test]
    fn snap_response_greeting_text_renders_content() {
        let body = snap_response_json("Hello, FID #42", "https://x.example/snaps/hello");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ui"]["elements"]["greeting"]["type"], "text");
        assert_eq!(
            v["ui"]["elements"]["greeting"]["props"]["content"],
            "Hello, FID #42"
        );
    }

    #[test]
    fn snap_response_button_props_match_spec() {
        let body =
            snap_response_json("Hello, @stranger", "https://master.wetware.run/snaps/hello");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let btn = &v["ui"]["elements"]["ping_button"];
        assert_eq!(btn["type"], "button");
        assert_eq!(btn["props"]["label"], "Ping me");
        assert_eq!(btn["props"]["variant"], "primary");
        // Label max 30 chars per spec — guard against future edits
        // that might overrun.
        let label = btn["props"]["label"].as_str().unwrap();
        assert!(label.chars().count() <= 30);
    }

    #[test]
    fn snap_response_button_press_action_is_submit_with_target() {
        let target = "https://master.wetware.run/snaps/hello";
        let body = snap_response_json("Hello, @stranger", target);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let press = &v["ui"]["elements"]["ping_button"]["on"]["press"];
        assert_eq!(press["action"], "submit");
        assert_eq!(press["params"]["target"], target);
    }

    #[test]
    fn snap_text_get_anonymous_returns_stranger() {
        // Clean env to avoid pollution from other tests (cargo runs
        // tests in the same process, so X_SNAP_FID_CLAIMED could leak).
        std::env::remove_var("X_SNAP_FID_CLAIMED");
        let s = snap_text("GET");
        assert_eq!(s, "Hello, @stranger");
    }

    #[test]
    fn snap_text_get_viewer_aware_renders_fid() {
        std::env::set_var("X_SNAP_FID_CLAIMED", "12345");
        let s = snap_text("GET");
        std::env::remove_var("X_SNAP_FID_CLAIMED");
        assert_eq!(s, "Hello, FID #12345");
    }

    #[test]
    fn snap_text_post_includes_timestamp_marker() {
        std::env::remove_var("X_SNAP_FID_CLAIMED");
        let s = snap_text("POST");
        assert!(s.starts_with("Hello, @stranger — pinged at "));
        assert!(s.ends_with(" UTC (unix)"));
    }

    #[test]
    fn snap_text_post_viewer_aware_includes_fid_and_timestamp() {
        std::env::set_var("X_SNAP_FID_CLAIMED", "7");
        let s = snap_text("POST");
        std::env::remove_var("X_SNAP_FID_CLAIMED");
        assert!(s.starts_with("Hello, FID #7 — pinged at "));
    }

    #[test]
    fn snap_response_text_content_under_320_chars() {
        // Worst-case: max-FID + POST timestamp.
        std::env::set_var("X_SNAP_FID_CLAIMED", "18446744073709551615");
        let body = snap_response_json(&snap_text("POST"), "https://x/snaps/hello");
        std::env::remove_var("X_SNAP_FID_CLAIMED");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let content = v["ui"]["elements"]["greeting"]["props"]["content"]
            .as_str()
            .unwrap();
        assert!(
            content.chars().count() <= 320,
            "text content must be <=320 chars per spec, got {}",
            content.chars().count()
        );
    }

    #[test]
    fn html_body_includes_greeting_and_doctype() {
        let body = html_body("@stranger");
        assert!(body.contains("<h1>Hello, @stranger</h1>"));
        assert!(body.contains("<!DOCTYPE html>"));
    }
}
