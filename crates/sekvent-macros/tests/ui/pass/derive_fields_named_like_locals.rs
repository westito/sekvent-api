//! `#[derive(ComponentError)]` on variants with fields named `error` and
//! `value`, like the locals of the generated code.

use sekvent_component::{AppError, ComponentError, ErrorCode};

#[derive(Debug, ComponentError)]
#[component_error(domain = "check.jobs.v1")]
pub enum JobsError {
    #[reason("JOB_FAILED", code = Aborted, message = "job {job} failed: {error}")]
    Failed {
        job: String,
        error: String,
        value: Option<u32>,
        hint: Option<String>,
    },
    #[reason("JOB_REJECTED", code = InvalidArgument)]
    Rejected { error: String },
    #[other]
    Other(AppError),
}

fn main() {
    let wire = JobsError::Failed {
        job: "nightly".to_owned(),
        error: "disk full".to_owned(),
        value: Some(7),
        hint: Some("free space".to_owned()),
    }
    .into_app_error();
    assert_eq!(wire.code(), ErrorCode::Aborted);
    assert_eq!(wire.message(), "job nightly failed: disk full");
    let metadata = wire.metadata();
    assert_eq!(metadata.get("error").map(String::as_str), Some("disk full"));
    assert_eq!(metadata.get("value").map(String::as_str), Some("7"));
    assert_eq!(metadata.get("hint").map(String::as_str), Some("free space"));
    match JobsError::from_app_error(wire) {
        JobsError::Failed {
            job,
            error,
            value,
            hint,
        } => {
            assert_eq!(job, "nightly");
            assert_eq!(error, "disk full");
            assert_eq!(value, Some(7));
            assert_eq!(hint.as_deref(), Some("free space"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let wire = AppError::from(JobsError::Failed {
        job: "weekly".to_owned(),
        error: "timeout".to_owned(),
        value: None,
        hint: None,
    });
    assert!(!wire.metadata().contains_key("value"));
    assert!(matches!(
        JobsError::from(wire),
        JobsError::Failed { value: None, hint: None, .. }
    ));

    let wire = JobsError::Rejected {
        error: "bad input".to_owned(),
    }
    .into_app_error();
    assert_eq!(wire.metadata().get("error").map(String::as_str), Some("bad input"));
    assert!(matches!(
        JobsError::from_app_error(wire),
        JobsError::Rejected { error } if error == "bad input"
    ));
}
