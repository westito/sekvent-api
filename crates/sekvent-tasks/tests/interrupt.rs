//! The Ctrl-C handler is process-wide, so it is tested in its own binary.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sekvent_tasks::install_interrupt_handler;

#[test]
fn the_handler_is_installed_once_and_records_sigint() {
    let flag = install_interrupt_handler();
    assert!(Arc::ptr_eq(&flag, &install_interrupt_handler()));
    assert!(!flag.load(Ordering::SeqCst));

    #[cfg(unix)]
    {
        use std::time::{Duration, Instant};

        let script = format!("kill -INT {}", std::process::id());
        let status = std::process::Command::new("sh")
            .args(["-c", &script])
            .status()
            .expect("sh");
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !flag.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "SIGINT was not recorded");
            std::thread::yield_now();
        }
    }
}
