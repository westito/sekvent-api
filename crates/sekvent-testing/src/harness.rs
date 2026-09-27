use std::time::{SystemTime, UNIX_EPOCH};

use crate::HarnessError;

/// The label namespace used unless a project picks its own.
pub const DEFAULT_NAMESPACE: &str = "io.sekvent.harness";

/// Environment variable that pins the run id, so a test runner can clean up
/// exactly the containers of the run it started.
pub const RUN_ID_ENV: &str = "SEKVENT_TEST_RUN_ID";

const MAX_LABEL_PART: usize = 128;

/// Identity stamped on every container the harness starts.
///
/// Every container gets three labels:
///
/// | label | value |
/// |---|---|
/// | `<ns>` | `true` |
/// | `<ns>.run` | the run id |
/// | `<ns>.created` | Unix seconds at start, for [`Harness::sweep_stale`] |
///
/// Cleanup only ever selects containers by these labels, never by image, so
/// it cannot touch a developer's own databases or another project's harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harness {
    namespace: String,
    run_id: String,
}

impl Harness {
    /// The default namespace and a run id from [`RUN_ID_ENV`], or a freshly
    /// generated one when that variable is unset or blank.
    pub fn from_env() -> Result<Self, HarnessError> {
        Self::with_run_id(DEFAULT_NAMESPACE, std::env::var(RUN_ID_ENV).ok().as_deref())
    }

    /// A harness under `namespace`, with a run id from [`RUN_ID_ENV`] or a
    /// generated one.
    pub fn with_namespace(namespace: &str) -> Result<Self, HarnessError> {
        Self::with_run_id(namespace, std::env::var(RUN_ID_ENV).ok().as_deref())
    }

    /// A harness with an explicit namespace and run id. `None` or a blank run
    /// id generates one with [`generate_run_id`].
    pub fn with_run_id(namespace: &str, run_id: Option<&str>) -> Result<Self, HarnessError> {
        validate_label_part("label namespace", namespace)?;
        let run_id = match run_id.map(str::trim) {
            Some(id) if !id.is_empty() => {
                validate_label_part("run id", id)?;
                id.to_owned()
            }
            _ => fresh_run_id(),
        };
        Ok(Self {
            namespace: namespace.to_owned(),
            run_id,
        })
    }

    /// The label namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// This process's run id.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The key of the run label, `<ns>.run`.
    pub fn run_label_key(&self) -> String {
        format!("{}.run", self.namespace)
    }

    /// The key of the creation-time label, `<ns>.created`.
    pub fn created_label_key(&self) -> String {
        format!("{}.created", self.namespace)
    }

    /// The labels for a container started now.
    pub fn labels(&self) -> Vec<(String, String)> {
        self.labels_at(SystemTime::now())
    }

    /// The labels for a container started at `now`.
    pub fn labels_at(&self, now: SystemTime) -> Vec<(String, String)> {
        vec![
            (self.namespace.clone(), "true".to_owned()),
            (self.run_label_key(), self.run_id.clone()),
            (self.created_label_key(), unix_seconds(now).to_string()),
        ]
    }

    /// `docker ps` filter selecting every container of this namespace.
    pub fn namespace_filter(&self) -> String {
        format!("label={}=true", self.namespace)
    }

    /// `docker ps` filter selecting the containers of one run.
    pub fn run_filter(&self, run_id: &str) -> String {
        format!("label={}={run_id}", self.run_label_key())
    }
}

/// A run id: Unix milliseconds, the process id and four random bytes, all in
/// hex, e.g. `18f3a2b4c5d-1a2b-0badf00d`.
///
/// The random part matters: test binaries in different containers share no
/// PID namespace, so two of them can start in the same millisecond with the
/// same pid.
pub fn generate_run_id(now: SystemTime, pid: u32, random: [u8; 4]) -> String {
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    format!("{millis:x}-{pid:x}-{:08x}", u32::from_be_bytes(random))
}

fn fresh_run_id() -> String {
    let random = uuid::Uuid::new_v4();
    let bytes = random.as_bytes();
    generate_run_id(
        SystemTime::now(),
        std::process::id(),
        [bytes[0], bytes[1], bytes[2], bytes[3]],
    )
}

pub(crate) fn unix_seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Label keys, run ids and container ids share one conservative alphabet so
/// they can be used as CLI arguments and in `docker ps` filters and format
/// templates without quoting.
pub(crate) fn validate_label_part(what: &'static str, value: &str) -> Result<(), HarnessError> {
    if value.is_empty() {
        return Err(HarnessError::Invalid {
            what,
            reason: "must not be empty",
        });
    }
    if value.len() > MAX_LABEL_PART {
        return Err(HarnessError::Invalid {
            what,
            reason: "is longer than 128 characters",
        });
    }
    if value.starts_with(['-', '.']) {
        return Err(HarnessError::Invalid {
            what,
            reason: "must not start with '-' or '.'",
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(HarnessError::Invalid {
            what,
            reason: "may only contain ASCII letters, digits, '.', '-' and '_'",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn run_ids_combine_time_pid_and_randomness() {
        let at = UNIX_EPOCH + Duration::from_millis(0x18f_3a2b_4c5d);
        assert_eq!(
            generate_run_id(at, 0x1a2b, [0x0b, 0xad, 0xf0, 0x0d]),
            "18f3a2b4c5d-1a2b-0badf00d"
        );
    }

    #[test]
    fn run_ids_before_the_epoch_clamp_to_zero() {
        let before = UNIX_EPOCH - Duration::from_secs(5);
        assert_eq!(generate_run_id(before, 1, [0; 4]), "0-1-00000000");
        assert_eq!(unix_seconds(before), 0);
    }

    #[test]
    fn generated_run_ids_are_valid_labels_and_differ() {
        let first = Harness::with_run_id(DEFAULT_NAMESPACE, None).unwrap();
        let second = Harness::with_run_id(DEFAULT_NAMESPACE, Some("  ")).unwrap();
        assert_ne!(first.run_id(), second.run_id());
        validate_label_part("run id", first.run_id()).unwrap();
        assert_eq!(first.run_id().split('-').count(), 3);
    }

    #[test]
    fn an_explicit_run_id_is_kept() {
        let harness = Harness::with_run_id("com.example.tests", Some("ci-42")).unwrap();
        assert_eq!(harness.namespace(), "com.example.tests");
        assert_eq!(harness.run_id(), "ci-42");
    }

    #[test]
    fn labels_carry_namespace_run_and_creation_time() {
        let harness = Harness::with_run_id("io.example", Some("r1")).unwrap();
        let labels = harness.labels_at(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(
            labels,
            vec![
                ("io.example".to_owned(), "true".to_owned()),
                ("io.example.run".to_owned(), "r1".to_owned()),
                ("io.example.created".to_owned(), "1700000000".to_owned()),
            ]
        );
        assert_eq!(harness.labels().len(), 3);
        assert_eq!(harness.namespace_filter(), "label=io.example=true");
        assert_eq!(harness.run_filter("r2"), "label=io.example.run=r2");
    }

    #[test]
    fn from_env_and_with_namespace_use_the_given_namespace() {
        assert_eq!(Harness::from_env().unwrap().namespace(), DEFAULT_NAMESPACE);
        assert_eq!(
            Harness::with_namespace("org.example").unwrap().namespace(),
            "org.example"
        );
    }

    #[test]
    fn bad_label_parts_are_rejected_with_a_reason() {
        for (value, reason) in [
            ("", "must not be empty"),
            ("-x", "must not start with '-' or '.'"),
            (".x", "must not start with '-' or '.'"),
            (
                "a b",
                "may only contain ASCII letters, digits, '.', '-' and '_'",
            ),
            (
                "a\"b",
                "may only contain ASCII letters, digits, '.', '-' and '_'",
            ),
        ] {
            let error = validate_label_part("label namespace", value).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("invalid label namespace: {reason}")
            );
        }
        let long = "a".repeat(129);
        assert!(validate_label_part("run id", &long).is_err());
        assert!(Harness::with_run_id("ok", Some("bad id")).is_err());
        assert!(Harness::with_run_id("bad ns", None).is_err());
    }
}
