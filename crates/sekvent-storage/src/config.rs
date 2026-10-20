use std::time::Duration;

use sekvent_config::{ConfigError, ConfigSource, EnvConfig, FromConfig, Prefixed, Secret};
use url::Url;

/// Settings of one bucket on an S3-compatible store.
///
/// The struct has no prefix of its own: read it under one you choose with
/// [`StorageConfig::load`] (e.g. `STORAGE_` for `STORAGE_ENDPOINT`), or nest
/// it in your own config struct with `#[config(nested)]`.
#[derive(Debug, EnvConfig)]
pub struct StorageConfig {
    /// S3 API endpoint the service calls, as `scheme://host[:port]`, e.g.
    /// `https://s3.eu-central-1.amazonaws.com` or `http://minio:9000`.
    #[config(validate = endpoint_url)]
    pub endpoint: String,
    /// Endpoint presigned URLs point at, when browsers reach the store at
    /// another address than the service does. Defaults to `ENDPOINT`.
    pub public_endpoint: Option<String>,
    /// Region the credentials sign for (`us-east-1` is `MinIO`'s default).
    #[config(validate = not_blank)]
    pub region: String,
    /// Bucket name.
    #[config(validate = valid_bucket)]
    pub bucket: String,
    /// Access key id.
    #[config(validate = not_blank)]
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: Secret,
    /// Session token of temporary credentials.
    pub session_token: Option<Secret>,
    /// Address the bucket in the path (`endpoint/bucket/key`, `MinIO` and
    /// most self-hosted stores) instead of the host
    /// (`bucket.endpoint/key`, AWS).
    #[config(default = "false")]
    pub path_style: bool,
    /// Timeout of each server-side request to the store.
    #[config(default = "30s", validate = positive)]
    pub timeout: Duration,
}

impl StorageConfig {
    /// Read the settings under `prefix` (used verbatim, include the
    /// separator), e.g. `STORAGE_`. Errors name the full key.
    pub fn load(source: &dyn ConfigSource, prefix: &str) -> Result<Self, ConfigError> {
        Self::from_config(&Prefixed::new(source, prefix))
    }
}

#[allow(clippy::ptr_arg)] // validators receive `&T`.
pub(crate) fn not_blank(value: &String) -> Result<(), String> {
    if value.trim().is_empty() {
        Err("must not be blank".into())
    } else {
        Ok(())
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
pub(crate) fn positive(value: &Duration) -> Result<(), String> {
    if value.is_zero() {
        Err("must be longer than zero".into())
    } else {
        Ok(())
    }
}

/// An endpoint reduced to what requests need: scheme and authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) scheme: String,
    /// `host[:port]`, the port only when it is not the scheme's default.
    pub(crate) authority: String,
}

/// Parse an endpoint: an absolute `http`/`https` URL with a host and
/// nothing else (no credentials, path, query or fragment).
pub(crate) fn parse_endpoint(value: &str) -> Result<Endpoint, String> {
    let url = Url::parse(value.trim()).map_err(|_| "must be an absolute URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("must be an http or https URL".into());
    }
    let Some(host) = url.host_str() else {
        return Err("must name a host".into());
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry credentials".into());
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err("must not have a path, query or fragment".into());
    }
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    Ok(Endpoint {
        scheme: url.scheme().to_owned(),
        authority,
    })
}

#[allow(clippy::ptr_arg)]
fn endpoint_url(value: &String) -> Result<(), String> {
    parse_endpoint(value).map(drop)
}

/// S3 bucket naming rules: 3 to 63 characters of lower-case letters,
/// digits, `-` and `.`, starting and ending with a letter or digit, no
/// `..`, and not shaped like an IPv4 address.
#[allow(clippy::ptr_arg)]
pub(crate) fn valid_bucket(value: &String) -> Result<(), String> {
    let bytes = value.as_bytes();
    let charset = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'));
    let edges = bytes
        .first()
        .zip(bytes.last())
        .is_some_and(|(first, last)| first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric());
    let ip_like = value.parse::<std::net::Ipv4Addr>().is_ok();
    if (3..=63).contains(&bytes.len()) && charset && edges && !value.contains("..") && !ip_like {
        Ok(())
    } else {
        Err("must be a valid S3 bucket name".into())
    }
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;

    use super::*;

    fn full() -> MapSource {
        MapSource::new()
            .with("STORAGE_ENDPOINT", "http://minio:9000")
            .with("STORAGE_PUBLIC_ENDPOINT", "https://files.example.com")
            .with("STORAGE_REGION", "us-east-1")
            .with("STORAGE_BUCKET", "attachments")
            .with("STORAGE_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .with("STORAGE_SECRET_ACCESS_KEY", "super-secret-value")
    }

    #[test]
    fn every_key_is_read_under_the_prefix() {
        let config = StorageConfig::load(&full(), "STORAGE_").unwrap();
        assert_eq!(config.endpoint, "http://minio:9000");
        assert_eq!(
            config.public_endpoint.as_deref(),
            Some("https://files.example.com")
        );
        assert_eq!(config.region, "us-east-1");
        assert_eq!(config.bucket, "attachments");
        assert_eq!(config.access_key_id, "AKIAEXAMPLE");
        assert_eq!(config.secret_access_key.expose(), "super-secret-value");
        assert!(config.session_token.is_none());
        assert!(!config.path_style);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert!(!format!("{config:?}").contains("super-secret-value"));
        let keys: Vec<String> = StorageConfig::keys().into_iter().map(|k| k.key).collect();
        assert_eq!(
            keys,
            [
                "ENDPOINT",
                "PUBLIC_ENDPOINT",
                "REGION",
                "BUCKET",
                "ACCESS_KEY_ID",
                "SECRET_ACCESS_KEY",
                "SESSION_TOKEN",
                "PATH_STYLE",
                "TIMEOUT"
            ]
        );
    }

    #[test]
    fn missing_and_invalid_keys_fail_closed_by_name() {
        let error = StorageConfig::load(&MapSource::new(), "STORAGE_").unwrap_err();
        let text = error.to_string();
        for key in [
            "STORAGE_ENDPOINT",
            "STORAGE_REGION",
            "STORAGE_BUCKET",
            "STORAGE_ACCESS_KEY_ID",
            "STORAGE_SECRET_ACCESS_KEY",
        ] {
            assert!(text.contains(key), "{key} in {text}");
        }
        let bad = full()
            .with("STORAGE_ENDPOINT", "ftp://minio")
            .with("STORAGE_BUCKET", "Bad_Bucket")
            .with("STORAGE_SECRET_ACCESS_KEY", " ")
            .with("STORAGE_TIMEOUT", "0s");
        let text = StorageConfig::load(&bad, "STORAGE_")
            .unwrap_err()
            .to_string();
        for key in [
            "STORAGE_ENDPOINT",
            "STORAGE_BUCKET",
            "STORAGE_SECRET_ACCESS_KEY",
            "STORAGE_TIMEOUT",
        ] {
            assert!(text.contains(key), "{key} in {text}");
        }
        assert!(!text.contains("ftp://minio"));
    }

    #[test]
    fn endpoints_keep_only_scheme_and_authority() {
        assert_eq!(
            parse_endpoint("http://minio:9000").unwrap(),
            Endpoint {
                scheme: "http".into(),
                authority: "minio:9000".into()
            }
        );
        assert_eq!(
            parse_endpoint("https://s3.amazonaws.com:443/").unwrap(),
            Endpoint {
                scheme: "https".into(),
                authority: "s3.amazonaws.com".into()
            }
        );
        assert_eq!(
            parse_endpoint("http://[::1]:9000").unwrap().authority,
            "[::1]:9000"
        );
        for (value, reason) in [
            ("minio:9000", "http or https"),
            ("not a url", "absolute URL"),
            ("ftp://host", "http or https"),
            ("http://user:pw@host", "credentials"),
            ("http://host/prefix", "path, query or fragment"),
            ("http://host/?a=b", "path, query or fragment"),
            ("http://host/#x", "path, query or fragment"),
        ] {
            let error = parse_endpoint(value).unwrap_err();
            assert!(error.contains(reason), "{value}: {error}");
        }
        assert!(endpoint_url(&"http://h".to_owned()).is_ok());
    }

    #[test]
    fn bucket_names_follow_s3_rules() {
        for name in ["abc", "my-bucket", "my.bucket.2", &"a".repeat(63)] {
            assert!(valid_bucket(&name.to_owned()).is_ok(), "{name}");
        }
        for name in [
            "ab",
            &"a".repeat(64),
            "Upper",
            "under_score",
            "-start",
            "end-",
            "a..b",
            "192.168.1.1",
            "",
        ] {
            assert!(valid_bucket(&name.to_owned()).is_err(), "{name}");
        }
    }

    #[test]
    fn validators_reject_blank_and_zero() {
        assert!(not_blank(&" ".to_owned()).is_err());
        assert!(not_blank(&"x".to_owned()).is_ok());
        assert!(positive(&Duration::ZERO).is_err());
        assert!(positive(&Duration::from_secs(1)).is_ok());
    }
}
