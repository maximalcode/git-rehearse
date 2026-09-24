//! Management must not remove a sandbox owned by another CLI process.
#![cfg(unix)]
mod support;

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use support::Fixture;

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", self.0.id())])
                .status();
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn discard_refuses_live_execution_then_succeeds_after_completion() {
    let fixture = Fixture::new();
    let (_, other, _) = fixture.rehearse(&["--json", "--keep", "merge", "feature"]);
    let other: serde_json::Value = serde_json::from_str(&other).unwrap();
    let marker = fixture.base().join("running");
    let release = fixture.base().join("release");
    let editor = fixture.base().join("editor.sh");
    std::fs::write(
        &editor,
        "touch \"$MARKER\"; while [ ! -f \"$RELEASE\" ]; do sleep 0.05; done\n",
    )
    .unwrap();
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
            .args(["--keep", "merge", "--no-ff", "--edit", "feature"])
            .current_dir(fixture.repo())
            .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
            .env("GIT_EDITOR", format!("sh '{}'", editor.display()))
            .env("MARKER", &marker)
            .env("RELEASE", &release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists(), "Git reached the blocking editor");
    let (code, listing, err) = fixture.rehearse(&["--json", "list"]);
    let listing: serde_json::Value = serde_json::from_str(&listing).unwrap();
    let entry = listing["rehearsals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] != other["id"])
        .unwrap();
    let id = entry["id"].as_str().unwrap();
    let original_refs = fixture.refs();
    let other_metadata = std::path::Path::new(other["storage"]["metadata"].as_str().unwrap());
    let other_bytes = std::fs::read(other_metadata).unwrap();
    let (discard_code, discard_out, _) = fixture.rehearse(&["--json", "discard", id]);
    assert_eq!(std::fs::read(other_metadata).unwrap(), other_bytes);
    // An independent rehearsal in the same repository remains usable while
    // this one is still executing.
    let (other_code, other_out, other_err) =
        fixture.rehearse(&["--json", "--keep", "merge", "feature"]);
    assert_eq!(other_code, 0, "{other_out}\n{other_err}");
    let independent: serde_json::Value = serde_json::from_str(&other_out).unwrap();
    assert_eq!(
        fixture
            .rehearse(&["--json", "discard", independent["id"].as_str().unwrap()])
            .0,
        0
    );
    assert_eq!(fixture.refs(), original_refs);
    assert!(child.0.try_wait().unwrap().is_none());
    let preserved = std::path::Path::new(entry["storage"]["metadata"].as_str().unwrap()).exists();
    std::fs::write(&release, "go").unwrap();
    let status = child.0.wait().unwrap();
    assert!(marker.exists());
    assert_eq!(code, 0, "{err}");
    assert_eq!(discard_code, 4, "{discard_out}");
    assert!(preserved);
    assert!(status.success());
    assert_eq!(entry["active"], true);
    assert_eq!(fixture.rehearse(&["--json", "discard", id]).0, 0);
    assert_eq!(
        fixture
            .rehearse(&["--json", "show", other["id"].as_str().unwrap()])
            .0,
        0
    );
}

#[test]
fn continuation_is_protected_and_crash_releases_ownership() {
    let fixture = Fixture::new();
    fixture.commit("diverged", "main version\n");
    let (code, report, err) = fixture.rehearse(&["--json", "--keep", "merge", "feature"]);
    assert_eq!(code, 2, "{err}");
    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    let id = report["id"].as_str().unwrap();
    let worktree = std::path::Path::new(report["sandbox"].as_str().unwrap());
    std::fs::write(worktree.join("file.txt"), "resolved\n").unwrap();
    fixture.git_in(worktree, &["add", "file.txt"]);
    let before = fixture.refs();
    let marker = fixture.base().join("continuing");
    let editor = fixture.base().join("editor.sh");
    std::fs::write(&editor, "touch \"$MARKER\"; exec sleep 60\n").unwrap();
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
            .args(["--keep", "continue", id])
            .current_dir(fixture.repo())
            .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
            .env("GIT_EDITOR", format!("sh '{}'", editor.display()))
            .env("MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.exists());
    assert!(child.0.try_wait().unwrap().is_none());
    for command in ["discard", "continue", "apply"] {
        let (code, out, err) = fixture.rehearse(&["--json", command, id]);
        assert_eq!(code, 4, "{command}: {out}\n{err}");
    }
    let (code, out, err) = fixture.rehearse(&["--json", "show", id]);
    assert_eq!(code, 0, "{err}");
    let shown: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(shown["active"], true);
    assert!(worktree.join("file.txt").exists());
    assert_eq!(fixture.refs(), before);
    // Kill the CLI before its descendants so it cannot record their failure.
    child.0.kill().unwrap();
    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.0.id())])
        .status()
        .unwrap();
    assert!(killed.success());
    child.0.wait().unwrap();
    let (_, out, _) = fixture.rehearse(&["--json", "list"]);
    let listed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(listed["rehearsals"][0]["active"], false);
    assert_eq!(listed["rehearsals"][0]["execution"], "incomplete");
    assert_eq!(fixture.rehearse(&["--json", "discard", id]).0, 0);
}

#[test]
fn construction_and_report_remain_reserved_against_pruning() {
    for stage in ["construction", "report"] {
        let fixture = Fixture::new();
        let marker = fixture.base().join("paused");
        let mut child = Running(
            Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
                .args(if stage == "construction" {
                    vec!["--json", "--keep", "merge", "feature"]
                } else {
                    vec!["--json", "merge", "feature"]
                })
                .current_dir(fixture.repo())
                .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
                .env(
                    "GIT_REHEARSE_PAUSE_EXECUTION_AT",
                    format!("{stage}={}", marker.display()),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "reached {stage}");
        let repo_dir = fixture
            .cache()
            .join(git_rehearse::cache::repo_id(fixture.repo()));
        let root = std::fs::read_dir(&repo_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        if stage == "report" {
            let id = root.file_name().unwrap().to_str().unwrap();
            let (code, out, err) = fixture.rehearse(&["--json", "discard", id]);
            assert_eq!(code, 4, "{out}\n{err}");
            // A recorded result does not release the report phase's ownership.
            let (_, out, _) = fixture.rehearse(&["--json", "show", id]);
            let shown: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(shown["active"], true);
            assert_eq!(shown["execution"], "clean");
        }
        assert!(
            git_rehearse::sandbox::prune(fixture.cache(), u64::MAX, 0)
                .unwrap()
                .is_empty()
        );
        assert!(root.exists());
        std::fs::remove_file(&marker).unwrap();
        assert!(child.0.wait().unwrap().success());
        let id = root.file_name().unwrap().to_str().unwrap();
        if stage == "construction" {
            assert_eq!(fixture.rehearse(&["--json", "discard", id]).0, 0);
        } else {
            assert!(
                !root.exists(),
                "unclaimed clean result is discarded after reporting"
            );
        }
    }
}
