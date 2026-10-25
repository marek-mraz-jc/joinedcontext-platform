//! HTTP header parsing every service shares, free of any feature so a crate without `auth`
//! reads a bearer token the same way the gateway does (T-5509).

/// The token in an `Authorization: Bearer <token>` header value, or `None` when there is no
/// header, another scheme or an empty token (PF-37). The scheme is matched case-insensitively,
/// as RFC 7235 section 2.1 requires.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let (scheme, token) = header?.trim_start().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::bearer;

    #[test]
    fn the_scheme_is_case_insensitive() {
        assert_eq!(bearer(Some("Bearer t")), Some("t"));
        assert_eq!(bearer(Some("bearer t")), Some("t"));
        assert_eq!(bearer(Some("BEARER t")), Some("t"));
        assert_eq!(bearer(Some("Bearer   t ")), Some("t"));
    }

    #[test]
    fn no_token_other_schemes_and_no_header_are_none() {
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(Some("Bearer")), None);
        assert_eq!(bearer(Some("Basic x")), None);
        assert_eq!(bearer(Some("Bearerx t")), None);
        assert_eq!(bearer(None), None);
    }
}
