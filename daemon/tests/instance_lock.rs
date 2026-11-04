// Keep this cross-process test in its own integration-test executable. In the
// parallel unit-test process, fork can inherit another test thread's open
// instance-lock descriptor until exec; CLOEXEC closes it at exec, not at fork.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use quotabard::runtime_state::{RuntimeState, RuntimeStateError};

static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const CHILD_HOME_ENV: &str = "QUOTABAR_RUNTIME_STATE_LOCK_CHILD_HOME";

struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        let sequence = HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quotabard-runtime-state-lock-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn process_lock_is_nonblocking_across_processes_and_released_on_drop() {
    let home = TestHome::new();
    let state = RuntimeState::open(home.path()).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("process_lock_child")
        .arg("--nocapture")
        .env(CHILD_HOME_ENV, home.path())
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed"));

    drop(state);
    assert!(RuntimeState::open(home.path()).is_ok());
}

#[test]
fn process_lock_child() {
    let Some(home) = std::env::var_os(CHILD_HOME_ENV) else {
        return;
    };
    let result = RuntimeState::open(Path::new(&home));
    assert!(matches!(result, Err(RuntimeStateError::AlreadyRunning)));
}
