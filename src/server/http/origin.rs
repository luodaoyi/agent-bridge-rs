/// Same-origin browser Origin on a loopback Host only.
pub(in crate::server) fn web_origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let Some(host) = host.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let Some(origin_authority) = http_origin_authority(origin) else {
        return false;
    };
    if !authority_is_loopback(&origin_authority) || !authority_is_loopback(host) {
        return false;
    }
    authority_eq(&origin_authority, host)
}

fn http_origin_authority(origin: &str) -> Option<String> {
    let rest = origin.strip_prefix("http://")?;
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return None;
    }
    Some(rest.to_owned())
}

fn authority_is_loopback(authority: &str) -> bool {
    let host = authority_host(authority);
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn authority_host(authority: &str) -> &str {
    if authority.starts_with('[') {
        if let Some(end) = authority.find(']') {
            return &authority[..=end];
        }
        return authority;
    }
    if let Some((host, port)) = authority.rsplit_once(':')
        && !host.is_empty()
        && port.chars().all(|ch| ch.is_ascii_digit())
    {
        return host;
    }
    authority
}

fn authority_eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}
