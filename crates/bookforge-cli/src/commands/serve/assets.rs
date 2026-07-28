use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/", get(index))
}

async fn index(State(state): State<AppState>) -> Response {
    (
        [
            ("content-security-policy", DASHBOARD_CONTENT_SECURITY_POLICY),
            ("x-frame-options", "DENY"),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
        ],
        Html(DASHBOARD_HTML.replace(CSRF_TOKEN_PLACEHOLDER, &state.csrf_token)),
    )
        .into_response()
}

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
