//! Undo selection, availability and crash recovery through the public JSON CLI.
mod support;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::Fixture;

fn command(f: &Fixture, repo: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_git-rehearse"));
    command
        .current_dir(repo)
        .arg("--json")
        .args(args)
        .env("GIT_REHEARSE_CACHE_DIR", f.cache())
        .env("GIT_EDITOR", "true");
    command
}

fn result(output: &Output) -> (i32, Value) {
    let value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    (output.status.code().expect("exit code"), value)
}

fn cli(f: &Fixture, repo: &Path, args: &[&str]) -> (i32, Value) {
    result(&command(f, repo, args).output().expect("CLI"))
}

fn success(f: &Fixture, repo: &Path, args: &[&str]) -> Value {
    let (code, value) = cli(f, repo, args);
    assert_eq!(code, 0, "{args:?}: {value}");
    value
}

fn apply(f: &Fixture, repo: &Path, branch: &str) -> String {
    let preview = success(f, repo, &["--keep", "merge", branch]);
    let id = preview["id"].as_str().unwrap().to_owned();
    success(f, repo, &["apply", &id]);
    id
}

fn admin(f: &Fixture, repo: &Path) -> PathBuf {
    PathBuf::from(f.git_in(repo, &["rev-parse", "--absolute-git-dir"]))
}

fn state(f: &Fixture, repo: &Path) -> (String, String, String, Vec<u8>) {
    (
        f.git_in(repo, &["show-ref", "--heads"]),
        f.git_in(repo, &["rev-parse", "HEAD"]),
        f.git_in(repo, &["write-tree"]),
        std::fs::read(repo.join("file.txt")).unwrap(),
    )
}

#[test]
fn availability_is_read_only_and_bound_to_the_last_apply_and_worktree() {
    for linked_origin in [false, true] {
        let f = Fixture::new();
        let linked = f.scratch("linked");
        f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
        let (origin, foreign) = if linked_origin {
            (linked.as_path(), f.repo())
        } else {
            (f.repo(), linked.as_path())
        };
        let empty = success(&f, origin, &["undo", "--check"]);
        assert_eq!(empty["available"], false);
        assert!(empty["rehearsal"].is_null());
        let id = apply(&f, origin, "feature");
        let before = state(&f, origin);
        let record_path = admin(&f, origin).join("rehearse-undo");
        let record = std::fs::read(&record_path).unwrap();
        let status = success(&f, origin, &["undo", &id, "--check"]);
        assert_eq!(status["available"], true, "{status}");
        assert_eq!(status["rehearsal"], id);
        assert_eq!(status["worktree"], origin.to_str().unwrap());
        assert!(status["applied_at_unix"].is_number());
        assert_eq!(state(&f, origin), before);
        assert_eq!(std::fs::read(&record_path).unwrap(), record);
        assert!(!f.repo().join(".git/rehearse-apply").exists());
        assert_eq!(
            success(&f, origin, &["undo", "wrong-id", "--check"])["available"],
            false
        );
        let foreign_before = state(&f, foreign);
        assert_eq!(cli(&f, foreign, &["undo", &id]).0, 4);
        // Even a copied record cannot authorize mutation from another worktree.
        std::fs::write(admin(&f, foreign).join("rehearse-undo"), &record).unwrap();
        assert_eq!(
            success(&f, foreign, &["undo", &id, "--check"])["available"],
            false
        );
        assert_eq!(cli(&f, foreign, &["undo", &id]).0, 4);
        assert_eq!(state(&f, foreign), foreign_before);
        assert_eq!(state(&f, origin), before);
        success(&f, origin, &["undo", &id]);
        assert_eq!(
            success(&f, origin, &["undo", "--check"])["available"],
            false
        );
    }
}

#[test]
fn unavailable_undo_preserves_changed_refs_staging_files_and_legacy_records() {
    for change in ["commit", "staged", "unstaged", "legacy", "foreign-checkout"] {
        let f = Fixture::new();
        let id = apply(&f, f.repo(), "feature");
        let record_path = admin(&f, f.repo()).join("rehearse-undo");
        match change {
            "commit" => {
                f.commit("new work", "new work\n");
            }
            "staged" | "unstaged" => {
                std::fs::write(f.repo().join("file.txt"), "new local work\n").unwrap();
                if change == "staged" {
                    f.git(&["add", "file.txt"]);
                }
            }
            "legacy" => {
                let text = std::fs::read_to_string(&record_path)
                    .unwrap()
                    .replace("version 2", "version 1");
                let text = text
                    .lines()
                    .filter(|line| !line.starts_with("origin "))
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(&record_path, text).unwrap();
            }
            "foreign-checkout" => {
                f.git(&["checkout", "feature"]);
                f.git(&[
                    "worktree",
                    "add",
                    f.scratch("linked").to_str().unwrap(),
                    "main",
                ]);
            }
            _ => unreachable!(),
        }
        let before = state(&f, f.repo());
        let record = std::fs::read(&record_path).unwrap();
        let status = success(&f, f.repo(), &["undo", &id, "--check"]);
        assert_eq!(status["available"], false, "{change}: {status}");
        assert!(status["reason"].is_string());
        assert_eq!(cli(&f, f.repo(), &["undo", &id]).0, 4);
        assert_eq!(state(&f, f.repo()), before, "{change}");
        assert_eq!(std::fs::read(&record_path).unwrap(), record);
    }
}

#[test]
fn every_undo_boundary_recovers_in_its_origin_and_keeps_foreign_work() {
    for linked_origin in [false, true] {
        for stage in [
            "before-journal",
            "after-journal",
            "after-ref-transaction",
            "after-refs-phase",
            "after-worktree-update",
            "after-worktree-phase",
            "after-undo-record-removal",
            "after-complete",
        ] {
            let f = Fixture::new();
            let linked = f.scratch("linked");
            f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
            let (origin, foreign) = if linked_origin {
                (linked.as_path(), f.repo())
            } else {
                (f.repo(), linked.as_path())
            };
            let original = state(&f, origin);
            let id = apply(&f, origin, "feature");
            std::fs::write(foreign.join("file.txt"), "foreign staged\n").unwrap();
            f.git_in(foreign, &["add", "file.txt"]);
            std::fs::write(foreign.join("file.txt"), "foreign unstaged\n").unwrap();
            let foreign_before = state(&f, foreign);
            let killed = command(&f, origin, &["undo", &id])
                .env("GIT_REHEARSE_ABORT_UNDO_AT", stage)
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&killed.stderr)
                    .contains(&format!("test abort reached: {stage}"))
            );
            if stage == "before-journal" {
                assert_eq!(
                    success(&f, origin, &["undo", &id, "--check"])["available"],
                    true
                );
                success(&f, origin, &["undo", &id]);
            } else {
                assert_eq!(
                    success(&f, origin, &["undo", &id, "--check"])["available"],
                    false
                );
                let status = success(&f, origin, &["recover", &id]);
                assert_eq!(status["operation"], "undo");
                assert_eq!(status["rehearsal"], id);
                let expected = match stage {
                    "after-journal" => "before_ref_change",
                    "after-ref-transaction" | "after-refs-phase" => "after_ref_change",
                    _ => "complete",
                };
                assert_eq!(status["state"], expected, "{stage}: {status}");
                assert_eq!(cli(&f, foreign, &["recover", &id, "--complete"]).0, 4);
                assert_eq!(cli(&f, foreign, &["--keep", "merge", "feature"]).0, 4);
                if stage == "after-journal" {
                    success(&f, origin, &["recover", &id, "--rollback"]);
                    assert_eq!(
                        success(&f, origin, &["undo", &id, "--check"])["available"],
                        true
                    );
                    success(&f, origin, &["undo", &id]);
                } else {
                    let before_recovery = state(&f, origin);
                    success(&f, origin, &["recover", &id, "--complete"]);
                    if expected == "complete" {
                        assert_eq!(state(&f, origin), before_recovery);
                    }
                }
            }
            assert_eq!(state(&f, origin), original, "{linked_origin}: {stage}");
            // Shared branch refs return to original; compare the foreign local state separately.
            let foreign_after = state(&f, foreign);
            assert_eq!(foreign_after.1, foreign_before.1);
            assert_eq!(foreign_after.2, foreign_before.2);
            assert_eq!(foreign_after.3, foreign_before.3);
            assert!(!admin(&f, origin).join("rehearse-undo").exists());
            assert!(!f.repo().join(".git/rehearse-apply").exists());
            assert_eq!(cli(&f, origin, &["undo", &id]).0, 4);
        }
    }
}

#[test]
fn a_new_apply_cannot_silently_replace_the_explicit_undo_target() {
    let f = Fixture::new();
    let first = apply(&f, f.repo(), "feature");
    assert_eq!(
        success(&f, f.repo(), &["undo", &first, "--check"])["available"],
        true
    );
    f.git(&["checkout", "feature"]);
    f.commit("next", "next\n");
    f.git(&["checkout", "main"]);
    let second = apply(&f, f.repo(), "feature");
    let before = state(&f, f.repo());
    let status = success(&f, f.repo(), &["undo", &first, "--check"]);
    assert_eq!(status["available"], false);
    assert_eq!(status["rehearsal"], second);
    assert_eq!(cli(&f, f.repo(), &["undo", &first]).0, 4);
    assert_eq!(state(&f, f.repo()), before);
    success(&f, f.repo(), &["undo", &second]);
}

#[test]
fn undo_availability_includes_untracked_and_ignored_collisions() {
    for ignored in [false, true] {
        let f = Fixture::new();
        f.git(&["checkout", "feature"]);
        f.git(&["rm", "file.txt"]);
        f.git(&["commit", "-m", "delete file"]);
        f.git(&["checkout", "main"]);
        let id = apply(&f, f.repo(), "feature");
        if ignored {
            std::fs::write(f.repo().join(".git/info/exclude"), "file.txt\n").unwrap();
        }
        std::fs::write(f.repo().join("file.txt"), "new untracked work\n").unwrap();
        let before = state(&f, f.repo());
        assert_eq!(
            success(&f, f.repo(), &["undo", &id, "--check"])["available"],
            false
        );
        assert_eq!(cli(&f, f.repo(), &["undo", &id]).0, 4);
        assert_eq!(state(&f, f.repo()), before);
        assert!(!f.repo().join(".git/rehearse-apply").exists());
    }
}

#[test]
fn new_work_during_interrupted_undo_blocks_both_recovery_directions() {
    let f = Fixture::new();
    let id = apply(&f, f.repo(), "feature");
    let killed = command(&f, f.repo(), &["undo", &id])
        .env("GIT_REHEARSE_ABORT_UNDO_AT", "after-ref-transaction")
        .output()
        .unwrap();
    assert!(!killed.status.success());
    std::fs::write(f.repo().join("file.txt"), "new work while interrupted\n").unwrap();
    let before = state(&f, f.repo());
    let journal_path = f.repo().join(".git/rehearse-apply");
    let journal = std::fs::read(&journal_path).unwrap();
    assert_eq!(
        success(&f, f.repo(), &["recover", &id])["state"],
        "ambiguous"
    );
    for action in ["--complete", "--rollback"] {
        assert_eq!(cli(&f, f.repo(), &["recover", &id, action]).0, 4);
        assert_eq!(state(&f, f.repo()), before);
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
    }
}

#[test]
fn interrupted_undo_rollback_can_itself_be_resumed_at_every_boundary() {
    for stage in [
        "after-rollback-journal",
        "after-rollback-refs",
        "after-rollback-worktree",
    ] {
        let f = Fixture::new();
        let id = apply(&f, f.repo(), "feature");
        let applied = state(&f, f.repo());
        let record_path = admin(&f, f.repo()).join("rehearse-undo");
        let record = std::fs::read(&record_path).unwrap();
        let killed = command(&f, f.repo(), &["undo", &id])
            .env("GIT_REHEARSE_ABORT_UNDO_AT", "after-worktree-update")
            .output()
            .unwrap();
        assert!(!killed.status.success());
        let killed = command(&f, f.repo(), &["recover", &id, "--rollback"])
            .env("GIT_REHEARSE_ABORT_RECOVERY_AT", stage)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&killed.stderr)
                .contains(&format!("test abort reached: {stage}"))
        );
        let status = success(&f, f.repo(), &["recover", &id]);
        assert_eq!(status["state"], "rolling_back", "{stage}: {status}");
        success(&f, f.repo(), &["recover", &id, "--rollback"]);
        assert_eq!(state(&f, f.repo()), applied);
        assert_eq!(std::fs::read(&record_path).unwrap(), record);
        assert_eq!(
            success(&f, f.repo(), &["undo", &id, "--check"])["available"],
            true
        );
    }
}

#[test]
fn a_live_undo_excludes_a_competing_apply_and_preserves_its_target() {
    let f = Fixture::new();
    let id = apply(&f, f.repo(), "feature");
    f.git(&["checkout", "feature"]);
    f.commit("next", "next\n");
    f.git(&["checkout", "main"]);
    let preview = success(&f, f.repo(), &["--keep", "merge", "feature"]);
    let next = preview["id"].as_str().unwrap();
    let marker = f.base().join("undo-paused");
    let mut undo = command(&f, f.repo(), &["undo", &id])
        .env(
            "GIT_REHEARSE_PAUSE_TRANSACTION_AT",
            format!("before-head-locks={}", marker.display()),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    while !marker.exists() {
        if undo.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
            let _ = undo.kill();
            panic!("Undo did not reach its transaction");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let before = state(&f, f.repo());
    let refused = cli(&f, f.repo(), &["apply", next]);
    std::fs::remove_file(marker).unwrap();
    let undone = result(&undo.wait_with_output().unwrap());
    assert_eq!(refused.0, 4, "{refused:?}");
    assert_eq!(undone.0, 0, "{undone:?}");
    assert_eq!(undone.1["rehearsal"], id);
    assert_ne!(state(&f, f.repo()).1, before.1);
    assert_eq!(
        cli(&f, f.repo(), &["apply", next]).0,
        4,
        "overlapping preview is stale after Undo"
    );
}

#[test]
fn independent_worktrees_keep_their_own_last_apply() {
    let f = Fixture::new();
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
    let main_id = apply(&f, f.repo(), "feature");
    let linked_id = apply(&f, &linked, "feature");
    assert_ne!(main_id, linked_id);
    assert_eq!(
        success(&f, f.repo(), &["undo", &main_id, "--check"])["available"],
        true
    );
    assert_eq!(
        success(&f, &linked, &["undo", &linked_id, "--check"])["available"],
        true
    );
    assert_eq!(cli(&f, &linked, &["undo", &main_id]).0, 4);
    success(&f, f.repo(), &["undo", &main_id]);
    assert_eq!(
        success(&f, &linked, &["undo", &linked_id, "--check"])["available"],
        true
    );
    success(&f, &linked, &["undo", &linked_id]);
}

#[test]
fn invalid_inspection_arguments_never_execute_undo() {
    let f = Fixture::new();
    let id = apply(&f, f.repo(), "feature");
    let before = state(&f, f.repo());
    for args in [vec!["undo", "--chek"], vec!["undo", &id, "wrong-id"]] {
        assert_eq!(cli(&f, f.repo(), &args).0, 4);
        assert_eq!(state(&f, f.repo()), before);
    }
    let output = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(f.repo())
        .args(["undo", &id, "--check"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Undo available"));
    assert_eq!(state(&f, f.repo()), before);
}
