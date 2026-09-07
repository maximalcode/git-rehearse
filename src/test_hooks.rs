//! Opt-in process coordination for integration tests against the real CLI.

/// Pause at a named stage until the test removes the configured marker.
pub(crate) fn pause(variable: &str, stage: &str) {
    let Ok(configured) = std::env::var(variable) else {
        return;
    };
    let Some((configured_stage, marker)) = configured.split_once('=') else {
        return;
    };
    if configured_stage != stage || marker.is_empty() {
        return;
    }
    let marker = std::path::Path::new(marker);
    if std::fs::write(marker, b"paused").is_err() {
        return;
    }
    while marker.exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Abort only when an integration test explicitly selects this stage.
pub(crate) fn abort(variable: &str, stage: &str) {
    if std::env::var(variable).ok().as_deref() == Some(stage) {
        eprintln!("git-rehearse test abort reached: {stage}");
        std::process::abort();
    }
}
