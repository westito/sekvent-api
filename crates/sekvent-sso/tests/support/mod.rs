//! Shared helpers: a minimal browser for the SSO routes.

#![allow(dead_code)]

use std::future::Future;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode, header};
use sekvent_config::Secret;
use tower::ServiceExt as _;

/// The application the browser returns to.
pub(crate) const APP: &str = "https://app.example.com";

/// A state key of the minimum length.
pub(crate) fn state_key() -> Secret {
    Secret::new("0123456789abcdef0123456789abcdef")
}

/// What a route answered.
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) location: String,
    pub(crate) set_cookie: Option<String>,
    pub(crate) cache_control: Option<String>,
    pub(crate) referrer_policy: Option<String>,
}

impl Answer {
    /// The `name=value` pair to send back, from `Set-Cookie`.
    pub(crate) fn cookie(&self) -> String {
        let set = self.set_cookie.as_deref().expect("a Set-Cookie header");
        set.split(';').next().unwrap().to_owned()
    }

    /// The fragment of `Location` as `(key, value)`.
    pub(crate) fn fragment(&self) -> (String, String) {
        let (_, fragment) = self.location.split_once('#').expect("a fragment");
        let (key, value) = fragment.split_once('=').expect("key=value");
        (key.to_owned(), value.to_owned())
    }

    /// `Location` without its fragment.
    pub(crate) fn target(&self) -> &str {
        self.location.split('#').next().unwrap()
    }

    /// A query parameter of `Location`.
    pub(crate) fn param(&self, name: &str) -> Option<String> {
        let url = url::Url::parse(&self.location).unwrap();
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    /// The error code in the fragment, asserting the response is an error
    /// redirect to `target`.
    pub(crate) fn error_to(&self, target: &str) -> String {
        assert_eq!(self.status, StatusCode::FOUND);
        assert_eq!(self.target(), format!("{APP}{target}"));
        let (key, value) = self.fragment();
        assert_eq!(key, "sso_error");
        value
    }
}

/// `GET uri` with an optional `Cookie` header.
pub(crate) async fn get(router: &Router, uri: &str, cookie: Option<&str>) -> Answer {
    let mut request = Request::get(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let text = |name| {
        response
            .headers()
            .get(name)
            .map(|value: &http::HeaderValue| value.to_str().unwrap().to_owned())
    };
    Answer {
        status: response.status(),
        location: text(header::LOCATION).unwrap_or_default(),
        set_cookie: text(header::SET_COOKIE),
        cache_control: text(header::CACHE_CONTROL),
        referrer_policy: text(header::REFERRER_POLICY),
    }
}

/// Fail instead of hanging: every socket test runs under this.
pub(crate) async fn guard<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("the test hung")
}
