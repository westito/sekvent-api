use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::harness::{unix_seconds, validate_label_part};
use crate::{Harness, HarnessError};

/// What a sweep removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Ids of the containers passed to `docker rm`.
    pub removed: Vec<String>,
}

impl Harness {
    /// Remove every container of run `run_id` in this harness's namespace.
    ///
    /// Selection is by label only (`<ns>=true` and `<ns>.run=<run_id>`),
    /// never by image.
    pub fn sweep_run(&self, run_id: &str) -> Result<SweepReport, HarnessError> {
        validate_label_part("run id", run_id)?;
        let listed = docker(&self.list_run_args(run_id))?;
        remove(parse_ids(&listed))
    }

    /// Remove this namespace's containers created more than `older_than`
    /// ago, sparing this harness's own run.
    ///
    /// Containers without a readable `<ns>.created` label are left alone.
    /// A too-short `older_than` can still hit another live run; pick one
    /// longer than your slowest test binary.
    pub fn sweep_stale(&self, older_than: Duration) -> Result<SweepReport, HarnessError> {
        self.sweep_stale_at(older_than, SystemTime::now())
    }

    /// [`Harness::sweep_stale`] with the current time passed in.
    pub fn sweep_stale_at(
        &self,
        older_than: Duration,
        now: SystemTime,
    ) -> Result<SweepReport, HarnessError> {
        let listed = docker(&self.list_all_args())?;
        remove(select_stale(&listed, self.run_id(), older_than, now))
    }

    fn list_run_args(&self, run_id: &str) -> Vec<String> {
        vec![
            "ps".to_owned(),
            "--all".to_owned(),
            "--quiet".to_owned(),
            "--filter".to_owned(),
            self.namespace_filter(),
            "--filter".to_owned(),
            self.run_filter(run_id),
        ]
    }

    fn list_all_args(&self) -> Vec<String> {
        vec![
            "ps".to_owned(),
            "--all".to_owned(),
            "--filter".to_owned(),
            self.namespace_filter(),
            "--format".to_owned(),
            format!(
                "{{{{.ID}}}}\t{{{{.Label \"{}\"}}}}\t{{{{.Label \"{}\"}}}}",
                self.created_label_key(),
                self.run_label_key()
            ),
        ]
    }
}

fn parse_ids(listed: &str) -> Vec<String> {
    listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Pick ids from `ID\tCREATED\tRUN` lines whose creation time is known and
/// older than the cutoff, and whose run is not `own_run`.
fn select_stale(listed: &str, own_run: &str, older_than: Duration, now: SystemTime) -> Vec<String> {
    let now = unix_seconds(now);
    let limit = older_than.as_secs();
    listed
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim().split('\t');
            let id = fields.next().filter(|id| !id.is_empty())?;
            let created: u64 = fields.next()?.trim().parse().ok()?;
            let run = fields.next().unwrap_or("").trim();
            let age = now.checked_sub(created)?;
            (age > limit && run != own_run).then(|| id.to_owned())
        })
        .collect()
}

fn remove(ids: Vec<String>) -> Result<SweepReport, HarnessError> {
    if ids.is_empty() {
        return Ok(SweepReport::default());
    }
    let mut args = vec![
        "rm".to_owned(),
        "--force".to_owned(),
        "--volumes".to_owned(),
    ];
    args.extend(ids.iter().cloned());
    docker(&args)?;
    Ok(SweepReport { removed: ids })
}

fn docker(args: &[String]) -> Result<String, HarnessError> {
    let output = Command::new("docker").args(args).output()?;
    if !output.status.success() {
        return Err(HarnessError::Docker(format!(
            "`docker {}` exited with {}: {}",
            args.first().map_or("", String::as_str),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;

    fn harness() -> Harness {
        Harness::with_run_id("io.example", Some("mine")).unwrap()
    }

    #[test]
    fn run_listing_filters_by_namespace_and_run_only() {
        assert_eq!(
            harness().list_run_args("r7"),
            [
                "ps",
                "--all",
                "--quiet",
                "--filter",
                "label=io.example=true",
                "--filter",
                "label=io.example.run=r7"
            ]
        );
    }

    #[test]
    fn stale_listing_selects_the_namespace_and_prints_labels() {
        let args = harness().list_all_args();
        assert_eq!(
            &args[..4],
            ["ps", "--all", "--filter", "label=io.example=true"]
        );
        assert_eq!(
            args[5],
            "{{.ID}}\t{{.Label \"io.example.created\"}}\t{{.Label \"io.example.run\"}}"
        );
        assert!(!args.iter().any(|arg| arg.contains("ancestor")));
    }

    #[test]
    fn ids_are_parsed_one_per_line() {
        assert_eq!(parse_ids("abc\n\n  def  \n"), ["abc", "def"]);
        assert!(parse_ids("").is_empty());
    }

    #[test]
    fn stale_selection_uses_age_run_and_known_creation_only() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let listed = "old\t1000\tother\n\
                      fresh\t9990\tother\n\
                      mine\t1000\tmine\n\
                      nolabel\t\tother\n\
                      future\t20000\tother\n\
                      norun\t1000\n\
                      \n";
        assert_eq!(
            select_stale(listed, "mine", Duration::from_secs(3600), now),
            ["old", "norun"]
        );
    }

    #[test]
    fn nothing_to_remove_does_not_call_docker() {
        assert_eq!(remove(Vec::new()).unwrap(), SweepReport::default());
    }

    #[test]
    fn bad_run_ids_are_refused() {
        assert!(harness().sweep_run("x y").is_err());
    }
}
