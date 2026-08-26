use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/", get(index))
}

/// The `/` bootstrap exchange (H-5 / SERVE-1).
///
/// Browsers follow the console-printed URL `http://127.0.0.1:PORT/?token=…`:
///
/// - a matching `?token=` gets this bootstrap page, which stores the token in
///   `sessionStorage` under the CSRF header name and immediately redirects to
///   the clean `/`. Every later API fetch sends it as the
///   `x-bookforge-csrf` header, exactly mirroring how the embedded CSRF token
///   used to be wired into `dashboard.js`.
/// - no token (or a wrong one) gets [`LOGIN_HTML`], which points at the
///   console URL and never echoes the session token itself.
/// - an authenticated caller that already holds the header may load the full
///   dashboard HTML directly.
async fn index(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query_token = query.as_deref().and_then(query_param_token);
    if !state.auth_enabled {
        // Old behavior restored by --no-auth: dashboard with the embedded
        // session-CSRF token, protected by loopback bind + Host allowlist +
        // per-mutation CSRF checks only.
        return Html(DASHBOARD_HTML.replace(CSRF_TOKEN_PLACEHOLDER, &state.csrf_token))
            .into_response();
    }

    if let Some(token) = query_token {
        if constant_time_eq(token.as_bytes(), state.csrf_token.as_bytes()) {
            return Html(BOOTSTRAP_HTML.replace("__BOOKFORGE_SESSION_TOKEN__", token))
                .into_response();
        }
        return unauthorized();
    }

    // Auth-on, no `?token=`: a caller already holding the header (scripts,
    // or the dashboard after a reload inside the seeded tab) gets the full
    // page; everyone else gets the pointer to the console-printed URL.
    let authorized = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| constant_time_eq(token.as_bytes(), state.csrf_token.as_bytes()));
    if authorized {
        return Html(DASHBOARD_HTML.replace(CSRF_TOKEN_PLACEHOLDER, &state.csrf_token))
            .into_response();
    }
    Html(LOGIN_HTML).into_response()
}

/// Extract a lone `token` parameter from a raw query string. Values are hex,
/// so percent-decoding is never needed; anything longer than a valid token
/// cannot match anyway and is simply compared and rejected.
fn query_param_token(query: &str) -> Option<&str> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .filter(|token| !token.is_empty())
}

const BOOTSTRAP_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Signing in to BookForge…</title>
</head>
<body>
<p style="font-family:system-ui,sans-serif">Signing in to the BookForge dashboard…</p>
<script>
(function () {
  try { sessionStorage.setItem("x-bookforge-csrf", "__BOOKFORGE_SESSION_TOKEN__"); } finally {
    location.replace("/");
  }
})();
</script>
<noscript><p>The BookForge dashboard requires JavaScript and the sign-in link printed in its console.</p></noscript>
</body>
</html>"#;

const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>BookForge dashboard</title>
</head>
<body style="font-family:system-ui,sans-serif;margin:4rem auto;max-width:36rem;line-height:1.5">
<h1>BookForge dashboard</h1>
<p>This dashboard is private. Open the <code>http://127.0.0.1:&lt;port&gt;/?token=…</code> link that
<code>bookforge serve</code> printed in its terminal — it carries your one-time session link.</p>
<p>If you started the server elsewhere, run <code>bookforge serve</code> again in a terminal you can see.</p>
</body>
</html>"#;

pub(super) const DASHBOARD_HTML_TEMPLATE: &str = include_str!("dashboard.html");
pub(super) const DASHBOARD_CSS: &str = include_str!("dashboard.css");
pub(super) const DASHBOARD_JS: &str = include_str!("dashboard.js");

pub(super) fn assemble_dashboard_html(template: &str, css: &str, js: &str) -> String {
    let template = template.replace("\r\n", "\n");
    let css = css.replace("\r\n", "\n");
    let js = js.replace("\r\n", "\n");

    template
        .replace("{{BOOKFORGE_DASHBOARD_CSS}}", &css)
        .replace("{{BOOKFORGE_DASHBOARD_JS}}", &js)
}

pub(super) static DASHBOARD_HTML: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    assemble_dashboard_html(DASHBOARD_HTML_TEMPLATE, DASHBOARD_CSS, DASHBOARD_JS)
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_page_wires_session_storage_with_csrf_header_name() {
        assert!(BOOTSTRAP_HTML.contains(CSRF_HEADER));
        assert!(BOOTSTRAP_HTML.contains("__BOOKFORGE_SESSION_TOKEN__"));
        assert!(!BOOTSTRAP_HTML.contains("__BOOKFORGE_SESSION_TOKEN___never_left"));
    }

    #[test]
    fn login_page_never_carries_a_token_placeholder() {
        assert!(!LOGIN_HTML.contains("__BOOKFORGE_SESSION_TOKEN__"));
        assert!(!LOGIN_HTML.contains(CSRF_TOKEN_PLACEHOLDER));
    }

    #[test]
    fn query_param_token_accepts_only_the_token_key() {
        assert_eq!(query_param_token("token=abcd"), Some("abcd"));
        assert_eq!(query_param_token("provider=mock&token=ef12"), Some("ef12"));
        assert_eq!(query_param_token("tokens=ef12"), None);
        assert_eq!(query_param_token(""), None);
    }
}
