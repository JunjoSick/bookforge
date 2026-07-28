use super::*;

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

pub(super) fn reject_mutation(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    if is_cross_site_browser_request(headers) {
        return Some(forbidden("cross-site dashboard request rejected"));
    }

    match headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        Some(token) if constant_time_eq(token.as_bytes(), state.csrf_token.as_bytes()) => None,
        _ => Some(forbidden("missing or invalid dashboard token")),
    }
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

fn is_cross_site_browser_request(headers: &HeaderMap) -> bool {
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
