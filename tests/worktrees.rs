//! Worktree safety through the public JSON CLI and real Git repositories.
mod support;
use std::path::Path;
use std::process::Command;
use support::Fixture;

fn cli(f: &Fixture, repo: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(repo)
        .args(["--json"])
        .args(args)
        .env("GIT_REHEARSE_CACHE_DIR", f.cache())
        .env("GIT_EDITOR", "true")
        .output()
        .expect("CLI runs");
    (
        out.status.code().expect("exit"),
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr))),
    )
}

#[test]
fn rehearsals_from_both_worktrees_preserve_the_other_worktree() {
    for linked_origin in [false, true] {
        let f = Fixture::new();
        let linked = f.scratch("linked");
        f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
        let (origin, foreign) = if linked_origin {
            (linked.as_path(), f.repo())
        } else {
            (f.repo(), linked.as_path())
        };
        std::fs::write(foreign.join("file.txt"), "foreign staged\n").unwrap();
        f.git_in(foreign, &["add", "file.txt"]);
        std::fs::write(foreign.join("file.txt"), "foreign unstaged\n").unwrap();
        let index = f.git_in(foreign, &["write-tree"]);
        let head = f.git_in(foreign, &["rev-parse", "HEAD"]);
        let (code, preview) = cli(&f, origin, &["--keep", "merge", "feature"]);
        assert_eq!(code, 0, "{preview}");
        let (code, applied) = cli(&f, origin, &["apply", preview["id"].as_str().unwrap()]);
        assert_eq!(code, 0, "{applied}");
        assert_eq!(
            f.git_in(origin, &["rev-parse", "HEAD"]),
            f.git(&["rev-parse", "feature"])
        );
        assert_eq!(f.git_in(foreign, &["rev-parse", "HEAD"]), head);
        assert_eq!(f.git_in(foreign, &["write-tree"]), index);
        assert_eq!(
            std::fs::read(foreign.join("file.txt")).unwrap(),
            b"foreign unstaged\n"
        );
    }
}

#[test]
fn a_branch_checked_out_elsewhere_after_preview_is_never_moved() {
    let f = Fixture::new();
    let (code, preview) = cli(
        &f,
        f.repo(),
        &["--keep", "--", "branch", "-f", "feature", "main"],
    );
    assert_eq!(code, 0, "{preview}");
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "feature"]);
    let before = f.git(&["rev-parse", "feature"]);
    let index = f.git_in(&linked, &["write-tree"]);
    let bytes = std::fs::read(linked.join("file.txt")).unwrap();
    let (code, result) = cli(&f, f.repo(), &["apply", preview["id"].as_str().unwrap()]);
    assert_eq!(code, 4, "{result}");
    assert_eq!(f.git(&["rev-parse", "feature"]), before);
    assert_eq!(f.git_in(&linked, &["write-tree"]), index);
    assert_eq!(std::fs::read(linked.join("file.txt")).unwrap(), bytes);
}

#[test]
fn missing_durable_origin_blocks_apply_and_preserves_the_preview() {
    let f = Fixture::new();
    let (_, preview) = cli(&f, f.repo(), &["--keep", "merge", "feature"]);
    let sandbox = Path::new(preview["sandbox"].as_str().unwrap());
    let metadata = sandbox.parent().unwrap().join("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata).unwrap()).unwrap();
    meta.as_object_mut().unwrap().remove("origin");
    std::fs::write(&metadata, serde_json::to_vec(&meta).unwrap()).unwrap();
    let head = f.git(&["rev-parse", "HEAD"]);
    let (code, result) = cli(&f, f.repo(), &["apply", preview["id"].as_str().unwrap()]);
    assert_eq!(code, 4, "{result}");
    assert_eq!(f.git(&["rev-parse", "HEAD"]), head);
    assert!(sandbox.exists());
}

#[test]
fn an_interrupted_linked_apply_blocks_mutation_in_the_main_worktree() {
    let f = Fixture::new();
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
    let (_, preview) = cli(&f, &linked, &["--keep", "merge", "feature"]);
    let output = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(&linked)
        .args(["--json", "apply", preview["id"].as_str().unwrap()])
        .env("GIT_REHEARSE_CACHE_DIR", f.cache())
        .env("GIT_REHEARSE_ABORT_APPLY_AT", "after-ref-transaction")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let (code, result) = cli(&f, f.repo(), &["--keep", "merge", "feature"]);
    assert_eq!(code, 4, "{result}");
    let (code, result) = cli(&f, &linked, &["recover", "--complete"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(
        f.git_in(&linked, &["rev-parse", "HEAD"]),
        f.git(&["rev-parse", "feature"])
    );
    assert_eq!(std::fs::read(f.repo().join("file.txt")).unwrap(), b"two\n");
}

#[test]
fn independent_results_apply_but_an_overlapping_result_stays_stale() {
    let f = Fixture::new();
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
    let (_, first) = cli(&f, f.repo(), &["--keep", "merge", "feature"]);
    let (_, overlap) = cli(&f, f.repo(), &["--keep", "merge", "feature"]);
    let (_, independent) = cli(&f, &linked, &["--keep", "merge", "feature"]);
    let (_, main_list) = cli(&f, f.repo(), &["list"]);
    let (_, linked_list) = cli(&f, &linked, &["list"]);
    assert_eq!(
        main_list["rehearsals"][0]["repository_id"],
        linked_list["rehearsals"][0]["repository_id"]
    );
    assert_ne!(
        main_list["rehearsals"][0]["origin_worktree"],
        linked_list["rehearsals"][0]["origin_worktree"]
    );
    let (code, result) = cli(&f, &linked, &["apply", first["id"].as_str().unwrap()]);
    assert_eq!(
        code, 4,
        "opening another worktree must not reassign the rehearsal: {result}"
    );
    assert_ne!(first["id"], independent["id"]);
    for (origin, preview) in [(f.repo(), &first), (linked.as_path(), &independent)] {
        let (code, result) = cli(&f, origin, &["apply", preview["id"].as_str().unwrap()]);
        assert_eq!(code, 0, "{result}");
    }
    let (code, result) = cli(&f, f.repo(), &["apply", overlap["id"].as_str().unwrap()]);
    assert_eq!(code, 4, "{result}");
    assert!(Path::new(overlap["sandbox"].as_str().unwrap()).exists());
}

#[test]
fn undo_refuses_a_branch_that_has_moved_to_a_foreign_checkout() {
    let f = Fixture::new();
    let (_, preview) = cli(&f, f.repo(), &["--keep", "merge", "feature"]);
    let id = preview["id"].as_str().unwrap();
    let (code, result) = cli(&f, f.repo(), &["apply", id]);
    assert_eq!(code, 0, "{result}");
    f.git(&["checkout", "--detach"]);
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "main"]);
    let before = f.git(&["rev-parse", "main"]);
    let (code, result) = cli(&f, f.repo(), &["undo", id]);
    assert_eq!(code, 4, "{result}");
    assert_eq!(f.git(&["rev-parse", "main"]), before);
    assert_eq!(std::fs::read(linked.join("file.txt")).unwrap(), b"three\n");
}

#[test]
fn linked_apply_transplants_the_reviewed_local_work_and_index() {
    let f = Fixture::new();
    f.commit_file("local.txt", "base\n", "local base");
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
    std::fs::write(linked.join("local.txt"), "staged\n").unwrap();
    f.git_in(&linked, &["add", "local.txt"]);
    std::fs::write(linked.join("local.txt"), "unstaged\n").unwrap();
    let (code, preview) = cli(&f, &linked, &["--keep", "merge", "--no-edit", "feature"]);
    assert_eq!(code, 0, "{preview}");
    let reviewed = Path::new(preview["sandbox"].as_str().unwrap());
    let reviewed_index = f.git_in(reviewed, &["write-tree"]);
    let (code, result) = cli(&f, &linked, &["apply", preview["id"].as_str().unwrap()]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(f.git_in(&linked, &["write-tree"]), reviewed_index);
    assert_eq!(
        std::fs::read(linked.join("local.txt")).unwrap(),
        b"unstaged\n"
    );
    assert_eq!(
        std::fs::read(f.repo().join("local.txt")).unwrap(),
        b"base\n"
    );
}

#[test]
fn external_checkout_and_a_second_apply_are_refused_while_the_first_is_paused() {
    let f = Fixture::new();
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
    let (_, preview) = cli(
        &f,
        f.repo(),
        &["--keep", "--", "branch", "-f", "feature", "main"],
    );
    let (_, second) = cli(&f, &linked, &["--keep", "merge", "feature"]);
    let marker = f.base().join("paused");
    let mut child = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(f.repo())
        .args(["--json", "apply", preview["id"].as_str().unwrap()])
        .env("GIT_REHEARSE_CACHE_DIR", f.cache())
        .env(
            "GIT_REHEARSE_PAUSE_APPLY_AT",
            format!("before-ref-transaction={}", marker.display()),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !marker.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "apply exited before the pause"
        );
        if start.elapsed() > std::time::Duration::from_mins(1) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("apply never reached the pause");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let (code, result) = cli(&f, &linked, &["apply", second["id"].as_str().unwrap()]);
    // Release before asserting so failures cannot leave a paused process.
    f.git_in(&linked, &["checkout", "feature"]);
    let before = f.git(&["rev-parse", "feature"]);
    std::fs::remove_file(&marker).unwrap();
    let first = child.wait_with_output().unwrap();
    assert_eq!(code, 4, "{result}");
    assert_eq!(
        first.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    assert_eq!(f.git(&["rev-parse", "feature"]), before);
    assert_eq!(std::fs::read(linked.join("file.txt")).unwrap(), b"three\n");
}

#[test]
fn a_detached_rebase_still_owns_its_original_branch() {
    let f = Fixture::new();
    f.commit("diverge", "main conflicts\n");
    let linked = f.scratch("linked");
    f.git(&["worktree", "add", linked.to_str().unwrap(), "feature"]);
    let rebase = Command::new("git")
        .arg("-C")
        .arg(&linked)
        .args(["rebase", "main"])
        .env("GIT_EDITOR", "true")
        .output()
        .unwrap();
    assert!(!rebase.status.success());
    let (_, preview) = cli(
        &f,
        f.repo(),
        &["--keep", "--", "branch", "-f", "feature", "main"],
    );
    let before = f.git(&["rev-parse", "feature"]);
    let admin = f.git_in(&linked, &["rev-parse", "--absolute-git-dir"]);
    let index = std::fs::read(Path::new(&admin).join("index")).unwrap();
    let bytes = std::fs::read(linked.join("file.txt")).unwrap();
    let (code, result) = cli(&f, f.repo(), &["apply", preview["id"].as_str().unwrap()]);
    assert_eq!(code, 4, "{result}");
    assert_eq!(f.git(&["rev-parse", "feature"]), before);
    assert_eq!(
        std::fs::read(Path::new(&admin).join("index")).unwrap(),
        index
    );
    assert_eq!(std::fs::read(linked.join("file.txt")).unwrap(), bytes);
}

#[test]
fn prepared_transactions_block_checkout_races_and_detect_new_worktrees() {
    for scenario in ["foreign-checkout", "new-worktree", "origin-checkout"] {
        let f = Fixture::new();
        let linked = f.scratch("linked");
        f.git(&["worktree", "add", linked.to_str().unwrap(), "-b", "other"]);
        let (_, preview) = cli(
            &f,
            f.repo(),
            &["--keep", "--", "branch", "-f", "feature", "main"],
        );
        let marker = f.base().join("transaction-paused");
        let mut child = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
            .current_dir(f.repo())
            .args(["--json", "apply", preview["id"].as_str().unwrap()])
            .env("GIT_REHEARSE_CACHE_DIR", f.cache())
            .env(
                "GIT_REHEARSE_PAUSE_TRANSACTION_AT",
                format!(
                    "{}={}",
                    if scenario == "origin-checkout" {
                        "before-head-locks"
                    } else {
                        "after-occupancy"
                    },
                    marker.display()
                ),
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_pause(&mut child, &marker);
        let before = f.git(&["rev-parse", "feature"]);
        let checkout = if scenario == "new-worktree" {
            Command::new("git")
                .arg("-C")
                .arg(f.repo())
                .args(["worktree", "add", "--no-checkout"])
                .arg(f.base().join("new-linked"))
                .arg("feature")
                .output()
                .unwrap()
        } else if scenario == "origin-checkout" {
            Command::new("git")
                .arg("-C")
                .arg(f.repo())
                .args(["checkout", "-b", "different"])
                .output()
                .unwrap()
        } else {
            Command::new("git")
                .arg("-C")
                .arg(&linked)
                .args(["checkout", "feature"])
                .output()
                .unwrap()
        };
        std::fs::remove_file(&marker).unwrap();
        let applied = child.wait_with_output().unwrap();
        if scenario == "foreign-checkout" {
            assert!(
                !checkout.status.success(),
                "foreign checkout must respect Git's HEAD lock"
            );
            assert_eq!(
                applied.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&applied.stdout)
            );
        } else {
            assert!(
                checkout.status.success(),
                "{}",
                String::from_utf8_lossy(&checkout.stderr)
            );
            assert_eq!(
                applied.status.code(),
                Some(4),
                "{}",
                String::from_utf8_lossy(&applied.stdout)
            );
            assert_eq!(f.git(&["rev-parse", "feature"]), before);
        }
        assert_eq!(
            f.git_in(&linked, &["symbolic-ref", "HEAD"]),
            "refs/heads/other"
        );
        assert!(
            !Path::new(&f.git_in(&linked, &["rev-parse", "--absolute-git-dir"]))
                .join("HEAD.lock")
                .exists()
        );
    }
}

#[test]
fn complex_revision_arguments_keep_the_conservative_branch_snapshot() {
    let f = Fixture::new();
    f.git(&["branch", "other", "main"]);
    let (code, preview) = cli(&f, f.repo(), &["--keep", "merge", "feature~0"]);
    assert_eq!(code, 0, "{preview}");
    f.git(&["branch", "-f", "other", "feature"]);
    let before = f.git(&["show-ref", "--heads"]);
    let bytes = std::fs::read(f.repo().join("file.txt")).unwrap();
    let (code, result) = cli(&f, f.repo(), &["apply", preview["id"].as_str().unwrap()]);
    assert_eq!(code, 4, "{result}");
    assert_eq!(f.git(&["show-ref", "--heads"]), before);
    assert_eq!(std::fs::read(f.repo().join("file.txt")).unwrap(), bytes);
}

fn wait_for_pause(child: &mut std::process::Child, marker: &Path) {
    let start = std::time::Instant::now();
    while !marker.exists() {
        if child.try_wait().unwrap().is_some()
            || start.elapsed() > std::time::Duration::from_mins(1)
        {
            let _ = child.kill();
            let status = child.wait().unwrap();
            panic!("no transaction pause: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
