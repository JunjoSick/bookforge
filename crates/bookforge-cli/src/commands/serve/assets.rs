use super::*;
use axum::http::HeaderValue;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/", get(index))
}

/// The `/` bootstrap exchange (H-5 / SERVE-1).
///
/// Browsers follow the console-printed URL `http://127.0.0.1:PORT/?token=…`:
///
/// - a matching `?token=` inside its five-minute TTL gets a 302 to the clean
///   `/` plus an HttpOnly `bookforge_session` cookie. The bootstrap token is
///   never echoed and never stored in the cookie — the cookie carries a fresh
///   session id minted by the exchange.
/// - an expired token gets the expiry-guidance page; a wrong token (or a
///   tripped failed-exchange limiter) gets a bare 401. Neither leaks anything.
/// - a caller that already holds the session cookie may load the full
///   dashboard HTML directly; everyone else gets the login page.
async fn index(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !state.auth.enabled {
        // Old behavior restored by --no-auth: dashboard with no embedded token
        // anywhere, protected by loopback bind + Host allowlist + per-mutation
        // same-origin checks only.
        return Html(DASHBOARD_HTML.clone()).into_response();
    }

    // A caller that already holds a live session is already in: reusing the
    // bootstrap link (or a stale copy of it) must neither re-run the one-time
    // exchange nor feed the failed-exchange limiter, so serve the dashboard.
    if state.auth.has_session(session_cookie_from(&headers)) {
        return Html(DASHBOARD_HTML.clone()).into_response();
    }

    if let Some(candidate) = query.as_deref().and_then(query_param_token) {
        // The exchange mints sessions and advances the failed-attempt limiter,
        // so like every other state change it is same-origin gated: only a
        // browser navigation (sec-fetch-site none/same-origin) or a same-origin
        // fetch may attempt it. A cross-site page must not be able to burn the
        // limiter or race the one-time token.
        if is_cross_site_browser_request(&headers) {
            return forbidden("cross-site dashboard request rejected");
        }
        return match state.auth.exchange_bootstrap(candidate) {
            BootstrapOutcome::Granted(session_id) => {
                let mut response = Response::new(axum::body::Body::empty());
                *response.status_mut() = StatusCode::FOUND;
                response
                    .headers_mut()
                    .insert("location", HeaderValue::from_static("/"));
                response
                    .headers_mut()
                    .insert("set-cookie", build_session_cookie(&session_id));
                response
            }
            BootstrapOutcome::Expired => Html(LOGIN_EXPIRED_HTML).into_response(),
            BootstrapOutcome::Rejected => unauthorized().into_response(),
        };
    }

    // No `?token=`: everyone else gets the pointer to the console-printed link.
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

const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>BookForge dashboard</title>
</head>
<body style="font-family:system-ui,sans-serif;margin:4rem auto;max-width:36rem;line-height:1.5">
<h1>BookForge dashboard</h1>
<p>This dashboard is private.</p>
<p>Open the sign-in link that <code>bookforge serve</code> printed in its terminal — it starts with
<code>http://127.0.0.1:&lt;port&gt;/?token=…</code> and carries your one-time session link.</p>
<p>If you started the server elsewhere, run <code>bookforge serve</code> again in a terminal you can see.</p>
</body>
</html>"#;

/// Shown when a sign-in link has outlived its five-minute bootstrap window.
/// Helpful, but never exposes the token or a session id.
const LOGIN_EXPIRED_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>BookForge dashboard — sign-in link expired</title>
</head>
<body style="font-family:system-ui,sans-serif;margin:4rem auto;max-width:36rem;line-height:1.5">
<h1>BookForge dashboard</h1>
<p>That sign-in link has expired — the link <code>bookforge serve</code> prints is only valid for
the first five minutes the server is running.</p>
<p>Run <code>bookforge serve</code> again in a terminal you can see and open the fresh link it prints.</p>
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

    /// The dashboard must be served byte-stable with no token/session
    /// placeholder left for the server to substitute — nothing secret is ever
    /// embedded in HTML or JS.
    #[test]
    fn dashboard_assets_carry_no_token_or_session_placeholders() {
        for secret_ish in [
            "sessionStorage",
            "x-bookforge-csrf",
            "__BOOKFORGE_",
            "bookforge_session=",
        ] {
            assert!(
                !DASHBOARD_HTML.contains(secret_ish),
                "dashboard must not embed {secret_ish:?}"
            );
        }
    }

    #[test]
    fn login_pages_never_carry_a_token_or_session_placeholder() {
        for page in [LOGIN_HTML, LOGIN_EXPIRED_HTML] {
            assert!(!page.contains("__BOOKFORGE_"));
            assert!(!page.contains("bookforge_session="));
            // The link text intentionally shows the URL shape (`?token=…`),
            // but no concrete value and no substitute-able placeholder.
            assert!(!page.contains("feedface"));
        }
    }

    #[test]
    fn query_param_token_accepts_only_the_token_key() {
        assert_eq!(query_param_token("token=abcd"), Some("abcd"));
        assert_eq!(query_param_token("provider=mock&token=ef12"), Some("ef12"));
        assert_eq!(query_param_token("tokens=ef12"), None);
        assert_eq!(query_param_token(""), None);
    }
}
