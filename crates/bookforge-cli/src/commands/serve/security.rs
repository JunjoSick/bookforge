use super::*;
use axum::http::HeaderValue;
use std::collections::VecDeque;
use std::time::Instant;

/// Name of the in-memory browser-session cookie issued by the `/` bootstrap
/// exchange. HttpOnly, SameSite=Strict, Path=/, browser-session lifetime (no
/// Max-Age) and deliberately no Secure flag because the loopback listener is
/// plain HTTP.
pub(super) const SESSION_COOKIE_NAME: &str = "bookforge_session";
const SESSION_COOKIE_PREFIX: &str = "bookforge_session=";

/// Bootstrap query tokens printed on the console are only accepted for the
/// first five minutes of server life.
const BOOTSTRAP_TTL: Duration = Duration::from_secs(5 * 60);
/// Bounded failed-exchange limiter: once this many wrong bootstrap tokens
/// arrive inside the TTL window, further exchanges are refused outright so a
/// probing local process cannot grind through the space.
const MAX_FAILED_BOOTSTRAP_EXCHANGES: usize = 8;
/// Cap on simultaneously remembered session ids; the oldest is evicted first.
const MAX_ACTIVE_SESSIONS: usize = 8;

/// Server-side authentication state for the loopback dashboard.
///
/// - The **bootstrap token** is the 128-bit secret printed in the console URL
///   (`?token=…`). It is valid for [`BOOTSTRAP_TTL`], compared in constant
///   time, and exchanged exactly once per successful handshake for a session
///   cookie. It is never stored in a cookie and never echoed in a response.
/// - The **session id** is a fresh random value minted per successful
///   exchange, held only in process memory (cleared on restart), compared in
///   constant time, and delivered to the browser as an HttpOnly cookie. It
///   never appears in HTML, JS, or JSON.
pub(super) struct AuthState {
    pub(super) enabled: bool,
    bootstrap: Mutex<Bootstrap>,
    sessions: Mutex<SessionStore>,
}

struct Bootstrap {
    value: String,
    valid_until: Instant,
    failed_exchanges: usize,
    /// Set on the first successful exchange. Once true the token is dead: a
    /// replayed or leaked console link can never mint a second session.
    consumed: bool,
}

#[derive(Default)]
struct SessionStore {
    ids: VecDeque<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BootstrapOutcome {
    /// The token matched inside its TTL; carries the freshly minted session id
    /// that the caller should hand the browser as the session cookie.
    Granted(String),
    /// Wrong token, an already-consumed token, or the failed-exchange limiter
    /// has already tripped.
    Rejected,
    /// The token is past its five-minute lifetime.
    Expired,
}

impl AuthState {
    pub(super) fn new(enabled: bool) -> Result<Self> {
        let token = generate_csrf_token()?;
        Ok(Self {
            enabled,
            bootstrap: Mutex::new(Bootstrap {
                value: token,
                valid_until: Instant::now() + BOOTSTRAP_TTL,
                failed_exchanges: 0,
                consumed: false,
            }),
            sessions: Mutex::new(SessionStore::default()),
        })
    }

    /// The console-printed bootstrap token (for the startup URL only).
    pub(super) fn bootstrap_token(&self) -> String {
        self.bootstrap
            .lock()
            .map(|bootstrap| bootstrap.value.clone())
            .unwrap_or_default()
    }

    /// Attempt the one-time bootstrap exchange. Only accepts the token during
    /// [`BOOTSTRAP_TTL`], compares in constant time, and bounds how many
    /// failed exchanges are entertained before refusing everything. The first
    /// successful exchange consumes the token: every later attempt — even with
    /// the exact console value — is refused, so a replayed or leaked link can
    /// never mint a second session.
    pub(super) fn exchange_bootstrap(&self, candidate: &str) -> BootstrapOutcome {
        let mut bootstrap = self.bootstrap.lock().unwrap_or_else(poisoned);
        if Instant::now() >= bootstrap.valid_until {
            return BootstrapOutcome::Expired;
        }
        if bootstrap.failed_exchanges >= MAX_FAILED_BOOTSTRAP_EXCHANGES {
            return BootstrapOutcome::Rejected;
        }
        if bootstrap.consumed {
            return BootstrapOutcome::Rejected;
        }
        if !constant_time_eq(candidate.as_bytes(), bootstrap.value.as_bytes()) {
            bootstrap.failed_exchanges += 1;
            return BootstrapOutcome::Rejected;
        }
        let Ok(session_id) = generate_csrf_token() else {
            return BootstrapOutcome::Rejected;
        };
        // Burn the token only after the session was actually minted, so a
        // failed random-id draw (Rejected above) does not consume a token the
        // operator could still use.
        bootstrap.consumed = true;
        self.sessions
            .lock()
            .unwrap_or_else(poisoned)
            .insert(session_id.clone());
        BootstrapOutcome::Granted(session_id)
    }

    /// True when `cookie` names a live in-memory session. Constant-time per
    /// stored id; a wrong or absent cookie never distinguishes its reason.
    pub(super) fn has_session(&self, cookie: Option<&str>) -> bool {
        match cookie {
            Some(cookie) => self
                .sessions
                .lock()
                .unwrap_or_else(poisoned)
                .contains(cookie),
            None => false,
        }
    }

    /// Forget the session behind `cookie` (logout). Idempotent for a missing
    /// or already-dead session.
    pub(super) fn end_session(&self, cookie: Option<&str>) {
        if let Some(cookie) = cookie {
            self.sessions.lock().unwrap_or_else(poisoned).remove(cookie);
        }
    }

    #[cfg(test)]
    pub(super) fn expire_bootstrap_for_test(&self) {
        self.bootstrap.lock().unwrap_or_else(poisoned).valid_until =
            Instant::now() - Duration::from_secs(1);
    }

    /// Test-only: pre-establish a session with a known id so router tests can
    /// authenticate by sending that value as the cookie without waiting for a
    /// real exchange.
    #[cfg(test)]
    pub(super) fn seed_session(&self, id: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(poisoned)
            .insert(id.to_string());
    }
}

fn poisoned<T>(poison: std::sync::PoisonError<T>) -> T {
    // A poisoned mutex means a thread panicked while holding the lock. The
    // auth data is still coherent (mutations complete before a panicking
    // thread drops the guard), so recover the inner value rather than fail
    // closed on the whole dashboard.
    poison.into_inner()
}

impl SessionStore {
    fn insert(&mut self, id: String) {
        if self.ids.iter().any(|existing| existing == &id) {
            return;
        }
        if self.ids.len() >= MAX_ACTIVE_SESSIONS {
            self.ids.pop_front();
        }
        self.ids.push_back(id);
    }

    fn contains(&self, candidate: &str) -> bool {
        self.ids
            .iter()
            .any(|existing| constant_time_eq(existing.as_bytes(), candidate.as_bytes()))
    }

    fn remove(&mut self, candidate: &str) -> bool {
        match self
            .ids
            .iter()
            .position(|existing| constant_time_eq(existing.as_bytes(), candidate.as_bytes()))
        {
            Some(index) => {
                self.ids.remove(index);
                true
            }
            None => false,
        }
    }
}

pub(super) async fn validate_dashboard_host(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if dashboard_host_allowed(request.headers(), state.host_port) {
        return next.run(request).await;
    }
    forbidden("dashboard host header rejected")
}

/// Bare 401 for failed session checks (H-5): no detail about which half of
/// the credential was wrong, so a probing local process learns nothing.
pub(super) fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "dashboard authentication required; open the URL printed by `bookforge serve`" })),
    )
        .into_response()
}

/// Stamp the hardened response headers onto every response (SERVE-9). The
/// index used to carry them alone; API bodies, error JSON, SSE frames, and
/// artifact streams deserve the same baseline defense.
pub(super) fn apply_security_headers(response: &mut Response) {
    let headers = response.headers_mut();
    for (name, value) in [
        ("content-security-policy", DASHBOARD_CONTENT_SECURITY_POLICY),
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ] {
        if let Ok(value) = value.parse::<HeaderValue>() {
            headers.insert(name, value);
        }
    }
}

fn dashboard_host_allowed(headers: &HeaderMap, port: u16) -> bool {
    headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| dashboard_host_value_allowed(host, port))
}

pub(super) fn dashboard_host_value_allowed(host: &str, port: u16) -> bool {
    if host.is_empty()
        || host
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return false;
    }

    if let Some(rest) = host.strip_prefix('[') {
        let Some((addr, suffix)) = rest.split_once(']') else {
            return false;
        };
        return addr == "::1" && suffix == format!(":{port}");
    }

    let Some((name, host_port)) = host.rsplit_once(':') else {
        return false;
    };
    if name.contains(':') || host_port != port.to_string() {
        return false;
    }

    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
}

pub(super) fn require_loopback_bind(addr: SocketAddr) -> Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    anyhow::bail!(
        "--bind must use a loopback address such as 127.0.0.1:8765; use an SSH tunnel for remote access"
    );
}

/// The mutation gate: same-origin (Host/Origin/Fetch-Metadata) checks plus, in
/// auth-on mode, a live browser session cookie. There is deliberately no
/// JavaScript-readable bearer token anymore — the HttpOnly session cookie is
/// the whole credential, so mutations are protected by SameSite=Strict plus
/// these explicit origin checks rather than an embedded CSRF token.
pub(super) fn reject_mutation(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    if is_cross_site_browser_request(headers) {
        return Some(forbidden("cross-site dashboard request rejected"));
    }

    if state.auth.enabled && !state.auth.has_session(session_cookie_from(headers)) {
        return Some(forbidden("missing or invalid dashboard session"));
    }
    None
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut diff = 0u8;
    for (left, right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

/// POST /api/auth/logout — the one session-management route exempt from the
/// authentication gate so a browser holding a stale cookie can clear it. It
/// still runs the same-origin gate, forgets the presented session, and sends
/// an expiring cookie.
pub(super) async fn auth_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if is_cross_site_browser_request(&headers) {
        return forbidden("cross-site dashboard request rejected");
    }
    state.auth.end_session(session_cookie_from(&headers));
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert("set-cookie", expire_session_cookie());
    response
}

/// Pull our session id out of a `Cookie` request header, if present.
pub(super) fn session_cookie_from(headers: &HeaderMap) -> Option<&str> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        part.trim()
            .strip_prefix(SESSION_COOKIE_PREFIX)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

/// The bootstrap-exchange success cookie: HttpOnly, SameSite=Strict, Path=/,
/// browser-session lifetime (no Max-Age), no Secure flag on loopback HTTP.
pub(super) fn build_session_cookie(value: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}={value}; HttpOnly; SameSite=Strict; Path=/"
    ))
    .expect("session cookie header is static and safe")
}

/// Expiring cookie sent on logout so the browser drops the session id.
pub(super) fn expire_session_cookie() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
    ))
    .expect("expiring session cookie header is static and safe")
}

pub(super) fn is_cross_site_browser_request(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|site| {
            site.eq_ignore_ascii_case("cross-site")
                || site.eq_ignore_ascii_case("same-site")
                || site.eq_ignore_ascii_case("none") && has_nonlocal_origin(headers)
        })
}

fn has_nonlocal_origin(headers: &HeaderMap) -> bool {
    headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| {
            !(origin.starts_with("http://127.0.0.1:")
                || origin.starts_with("http://localhost:")
                || origin.starts_with("http://[::1]:"))
        })
}

pub(super) fn generate_csrf_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("failed to generate dashboard token")?;
    Ok(hex_bytes(&bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to string cannot fail");
    }
    out
}
