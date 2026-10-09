//! Return paths and the application URL.

use sekvent_error::AppError;
use url::Url;

/// Longest accepted return path, in bytes.
pub const MAX_REDIRECT_LEN: usize = 2048;

/// Whether `path` is a safe same-origin return path: it starts with one `/`
/// (not `//`), has only printable ASCII without spaces, no `\` and no `#`,
/// and is at most [`MAX_REDIRECT_LEN`] bytes.
///
/// Control characters are refused because browsers strip tabs and line
/// breaks from URLs (`/\t/evil.example` would become `//evil.example`),
/// backslashes because browsers read them as `/`, and `#` because the
/// router appends its own fragment.
pub fn is_safe_redirect(path: &str) -> bool {
    path.len() <= MAX_REDIRECT_LEN
        && path.starts_with('/')
        && !path.starts_with("//")
        && path
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'\\' && b != b'#')
}

/// The application's base URL: absolute `http` or `https` with a host, no
/// query, fragment or user info. Returned without a trailing `/`, ready for
/// a return path to be appended.
pub(crate) fn parse_app_url(raw: &str) -> Result<String, AppError> {
    let invalid = || {
        AppError::invalid_argument(
            "the SSO application URL must be an absolute http(s) URL without query or fragment",
        )
    };
    let url = Url::parse(raw).map_err(|_| invalid())?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if !plain {
        return Err(invalid());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Whether `url` is plain `http` to a host other than a loopback one.
pub(crate) fn is_insecure(url: &Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(domain)) => !domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => !ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => !ip.is_loopback(),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_safe() {
        for path in [
            "/",
            "/orders",
            "/orders/7?tab=items&x=%2F%2Fy",
            "/a/b/",
            "/@evil.example",
        ] {
            assert!(is_safe_redirect(path), "{path}");
        }
    }

    #[test]
    fn open_redirects_are_refused() {
        for path in [
            "",
            "orders",
            "//evil.example",
            "//evil.example/x",
            "/\\evil.example",
            "\\\\evil.example",
            "https://evil.example",
            "http:/evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            "/\r/evil.example",
            "/ /evil.example",
            "/x#frag",
            "/caf\u{e9}",
            "javascript:alert(1)",
        ] {
            assert!(!is_safe_redirect(path), "{path:?}");
        }
        assert!(!is_safe_redirect(&format!(
            "/{}",
            "a".repeat(MAX_REDIRECT_LEN)
        )));
        assert!(is_safe_redirect(&format!(
            "/{}",
            "a".repeat(MAX_REDIRECT_LEN - 1)
        )));
    }

    #[test]
    fn app_urls() {
        assert_eq!(
            parse_app_url("https://app.example.com/").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            parse_app_url("http://127.0.0.1:5173/console").unwrap(),
            "http://127.0.0.1:5173/console"
        );
        for raw in [
            "app.example.com",
            "/relative",
            "ftp://app.example.com",
            "https://app.example.com/?a=1",
            "https://app.example.com/#x",
            "https://user:pw@app.example.com",
            "file:///tmp",
        ] {
            let error = parse_app_url(raw).unwrap_err();
            assert!(!error.message().contains(raw), "{raw}");
        }
    }

    #[test]
    fn insecure_urls() {
        let insecure = |raw: &str| is_insecure(&Url::parse(raw).unwrap());
        assert!(!insecure("https://bitbucket.org"));
        assert!(!insecure("http://localhost:8080/x"));
        assert!(!insecure("http://127.0.0.1:1/x"));
        assert!(!insecure("http://[::1]:1/x"));
        assert!(insecure("http://bitbucket.org"));
        assert!(insecure("http://10.0.0.1"));
        assert!(insecure("http://[2001:db8::1]"));
    }
}
