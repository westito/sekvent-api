//! Docker-backed tests.
//!
//! Run them with a reachable Docker daemon:
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-testing --all-features -- --ignored
//! ```
//!
//! Without `SEKVENT_DOCKER_TESTS=1` each test returns immediately, so
//! `--ignored` alone never needs Docker.

use std::process::Command;
use std::time::Duration;

use sekvent_testing::{Harness, Reaper, await_until, docker_tests_enabled};

fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

fn container_exists(id: &str) -> bool {
    Command::new("docker")
        .args(["container", "inspect", "--format", "{{.Id}}", id])
        .output()
        .expect("run docker inspect")
        .status
        .success()
}

/// A stopped container that carries this harness's labels, so any leftover
/// is still caught by the label-scoped sweep.
fn create_labelled_sentinel(harness: &Harness) -> String {
    let mut args = vec!["create".to_owned()];
    for (key, value) in harness.labels() {
        args.push("--label".to_owned());
        args.push(format!("{key}={value}"));
    }
    args.push("postgres:17-alpine".to_owned());
    args.push("true".to_owned());
    let output = Command::new("docker")
        .args(&args)
        .output()
        .expect("run docker create");
    assert!(output.status.success(), "docker create succeeds");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_reaper_removes_the_container_once_its_handle_drops() {
    if skip() {
        return;
    }
    let harness = Harness::with_namespace("io.sekvent.harness.selftest").unwrap();
    let id = create_labelled_sentinel(&harness);
    assert!(container_exists(&id));

    let reaper = Reaper::spawn(&id).unwrap();
    drop(reaper);

    await_until!(
        !tokio::task::spawn_blocking({
            let probe = id.clone();
            move || container_exists(&probe)
        })
        .await
        .unwrap(),
        timeout = Duration::from_secs(30),
        interval = Duration::from_millis(100),
    );
    assert!(!container_exists(&id));
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn sweep_run_removes_only_the_named_run() {
    if skip() {
        return;
    }
    let ns = "io.sekvent.harness.selftest";
    let mine = Harness::with_run_id(ns, Some(&format!("sweep-a-{}", std::process::id()))).unwrap();
    let other = Harness::with_run_id(ns, Some(&format!("sweep-b-{}", std::process::id()))).unwrap();
    let doomed = create_labelled_sentinel(&mine);
    let spared = create_labelled_sentinel(&other);

    let report = mine.sweep_run(mine.run_id()).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert!(!container_exists(&doomed));
    assert!(container_exists(&spared));

    other.sweep_run(other.run_id()).unwrap();
    assert!(!container_exists(&spared));
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn postgres_databases_are_separate_and_reachable() {
    use sekvent_testing::PostgresHarness;
    use sqlx::{Connection, PgConnection};

    if skip() {
        return;
    }
    let pg = PostgresHarness::shared().await;
    assert!(std::ptr::eq(pg, PostgresHarness::shared().await));
    let first = pg.create_database().await.unwrap();
    let second = pg.create_database().await.unwrap();
    assert_ne!(first.name, second.name);

    let mut a = PgConnection::connect(&first.url).await.unwrap();
    sqlx::raw_sql("CREATE TABLE marker (id INT)")
        .execute(&mut a)
        .await
        .unwrap();
    let mut b = PgConnection::connect(&second.url).await.unwrap();
    let visible: Option<String> = sqlx::query_scalar("SELECT to_regclass('marker')::text")
        .fetch_one(&mut b)
        .await
        .unwrap();
    assert_eq!(visible, None, "databases do not share tables");

    let fsync: String = sqlx::query_scalar("SHOW fsync")
        .fetch_one(&mut b)
        .await
        .unwrap();
    assert_eq!(fsync, "off");
    a.close().await.unwrap();
    b.close().await.unwrap();
    assert!(pg.url().starts_with("postgres://"));
    assert!(pg.port() > 0);
    assert!(!pg.host().is_empty());
    assert_eq!(
        format!("{first:?}"),
        format!("TestDatabase {{ name: {:?}, .. }}", first.name)
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_postgres_admin_password_is_random_per_server() {
    use sekvent_testing::PostgresHarness;
    use sqlx::{Connection, PgConnection};

    if skip() {
        return;
    }
    let pg = PostgresHarness::shared().await;
    let guessed = format!(
        "postgres://sekvent:sekvent@{}:{}/postgres",
        pg.host(),
        pg.port()
    );
    assert!(PgConnection::connect(&guessed).await.is_err());
    let admin = pg.admin_url();
    assert!(!admin.contains(":sekvent@"));
    let shown = format!("{pg:?}");
    let password = admin
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .and_then(|(user_info, _)| user_info.split_once(':'))
        .map(|(_, password)| password.to_owned())
        .unwrap();
    assert_eq!(password.len(), 32);
    assert!(!shown.contains(&password));
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_dedicated_postgres_container_carries_the_harness_labels() {
    use sekvent_testing::PostgresHarness;

    if skip() {
        return;
    }
    let harness = Harness::with_run_id(
        "io.sekvent.harness.selftest",
        Some(&format!("labels-{}", std::process::id())),
    )
    .unwrap();
    let pg = PostgresHarness::start(&harness, PostgresHarness::default_image())
        .await
        .unwrap();
    let listed = Command::new("docker")
        .args([
            "ps",
            "--quiet",
            "--filter",
            &harness.run_filter(harness.run_id()),
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&listed.stdout).lines().count(), 1);
    drop(pg);
    harness.sweep_run(harness.run_id()).unwrap();
}

#[cfg(feature = "mysql")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn mysql_databases_are_separate_and_reachable() {
    use sekvent_testing::MySqlHarness;
    use sqlx::{Connection, MySqlConnection};

    if skip() {
        return;
    }
    let my = MySqlHarness::shared().await;
    let first = my.create_database().await.unwrap();
    let second = my.create_database().await.unwrap();
    assert_ne!(first.name, second.name);

    let mut a = MySqlConnection::connect(&first.url).await.unwrap();
    sqlx::raw_sql("CREATE TABLE marker (id INT)")
        .execute(&mut a)
        .await
        .unwrap();
    let mut b = MySqlConnection::connect(&second.url).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = DATABASE() AND table_name = 'marker'",
    )
    .fetch_one(&mut b)
    .await
    .unwrap();
    assert_eq!(count, 0, "databases do not share tables");
    a.close().await.unwrap();
    b.close().await.unwrap();
    assert!(my.url().starts_with("mysql://"));
    assert!(my.admin_url().ends_with("/mysql"));

    let guessed = format!("mysql://root:sekvent@{}:{}/mysql", my.host(), my.port());
    assert!(MySqlConnection::connect(&guessed).await.is_err());
    assert!(!my.admin_url().contains(":sekvent@"));
    assert!(!format!("{my:?}").contains("mysql://"));
}
