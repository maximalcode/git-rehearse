//! Applying a rehearsal to a real repository.
//!
//! Design principle 2 is inviolable — **apply is a ref transplant, never a
//! re-run** — so most of these tests exist to prove that in ways a re-run
//! could not fake, and the rest exist to prove that a repository which moved
//! on is left alone.

mod support;

use git_rehearse::sandbox::{Plan, Sandbox};
use git_rehearse::{Error, apply, carry, execute, preflight, sandbox};
use support::Fixture;

const NOW: u64 = 1_786_248_000;

fn plan_of(fixture: &Fixture, command: &[&str]) -> Plan {
    preflight::run(fixture.repo())
        .expect("the fixture passes preflight")
        .into_plan(command.iter().map(|arg| (*arg).to_owned()).collect())
}

/// Rehearses `command` and hands back the sandbox, ready to apply.
fn rehearse(fixture: &Fixture, command: &[&str]) -> Sandbox {
    let plan = plan_of(fixture, command);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    execute::run(&sandbox.worktree(), &plan.command, None).expect("the command runs");
    sandbox
}

fn refusal(error: Error) -> String {
    match error {
        Error::Refused(message) => message,
        other => panic!("expected a refusal, got: {other:?}"),
    }
}

#[test]
fn the_repository_ends_up_at_exactly_the_rehearsed_commits() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let rehearsed = fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]);

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        fixture.git(&["rev-parse", "main"]),
        rehearsed,
        "the same commit id, not an equivalent one"
    );
    assert_eq!(applied.moved.len(), 2, "main and HEAD: {:?}", applied.moved);
}

#[test]
fn apply_transplants_and_cannot_be_re_running_the_command() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let worktree = sandbox.worktree();

    // Give the rehearsed commit a message that re-running `git merge` could
    // never produce. If apply re-ran the command, this would be gone.
    fixture.git_in(
        &worktree,
        &["commit", "--amend", "-m", "REHEARSED-NOT-RERUN"],
    );
    let rehearsed = fixture.git_in(&worktree, &["rev-parse", "main"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(fixture.git(&["rev-parse", "main"]), rehearsed);
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%s", "main"]),
        "REHEARSED-NOT-RERUN",
        "the applied commit is the object that was inspected, byte for byte"
    );
}

#[test]
fn the_worktree_is_brought_to_the_rehearsed_content() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let sandbox = rehearse(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();
    // Finish the rebase the fixture's conflict stopped.
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(applied.reset.as_deref(), Some("feature"));
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "resolved\n",
        "the checked-out branch was rewritten, so the worktree follows it"
    );
    assert_eq!(
        fixture.git(&["status", "--porcelain"]),
        "",
        "and the index agrees with it"
    );
}

#[test]
fn a_commit_made_since_the_rehearsal_stops_everything() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    // Someone — or the user in another terminal — commits meanwhile.
    fixture.commit_file("later.txt", "later\n", "five");
    let before = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("refs/heads/main is now"), "{message}");
    assert!(message.contains("Rehearse again"), "{message}");
    assert_eq!(
        fixture.refs(),
        before,
        "a refused apply must leave the repository exactly as it found it"
    );
}

#[test]
fn switching_branches_between_rehearsing_and_applying_is_refused() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    fixture.git(&["checkout", "feature"]);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("you rehearsed on main"), "{message}");
    assert!(message.contains("now on feature"), "{message}");
}

#[test]
fn uncommitted_work_is_never_destroyed_by_an_apply() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let sandbox = rehearse(&fixture, &["rebase", "main"]);
    let worktree = sandbox.worktree();
    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&worktree, &["add", "file.txt"]);
    fixture.git_in(&worktree, &["rebase", "--continue"]);
    // The user starts editing while reading the report.
    fixture.write("file.txt", "work in progress\n");
    let before = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("reset --hard"), "{message}");
    assert!(message.contains("commit or stash them"), "{message}");
    assert!(
        message.contains("not there when you rehearsed"),
        "the rehearsal carried nothing, so this edit appeared afterwards and is nobody's \
         business but the user's: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "work in progress\n",
        "the edit is still there"
    );
    assert_eq!(fixture.refs(), before, "and nothing moved");
}

#[test]
fn the_undo_record_is_written_with_both_sides_of_every_move() {
    // Both sides, because undo has to state the value it expects to replace or
    // it cannot have the guarantee apply has — see `undo.rs`, which owns the
    // format and proves the restore itself works.
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.git(&["rev-parse", "main"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    let record = std::fs::read_to_string(&applied.undo).expect("the undo file exists");
    let after = fixture.git(&["rev-parse", "main"]);
    assert!(
        record.contains(&format!("refs/heads/main {before} {after}")),
        "{record}"
    );
    assert!(record.contains(sandbox.id()), "{record}");
    assert!(record.contains("version 1"), "{record}");
    assert!(
        record.contains("git update-ref"),
        "the record should say how to use it: {record}"
    );

    // Written before anything moved, so a crash between here and there still
    // leaves the way back on disk.
    assert_ne!(after, before);
}

#[test]
fn the_reflog_says_where_the_new_commits_came_from() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    let reflog = fixture.git(&["reflog", "show", "--format=%gs", "main"]);
    assert!(
        reflog.contains(&format!("git-rehearse apply {}", sandbox.id())),
        "someone looking for what happened to their branch should find it: {reflog}"
    );
}

#[test]
fn the_rehearsed_commits_are_anchored_against_garbage_collection() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);
    let rehearsed = fixture.git_in(&sandbox.worktree(), &["rev-parse", "main"]);

    let applied = apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        fixture.git(&["rev-parse", &format!("{}main", applied.anchor)]),
        rehearsed,
        "the transplanted commits keep a ref of their own"
    );
    // And they survive the sandbox being thrown away, which is the point.
    sandbox.discard().expect("discard");
    fixture.git(&["cat-file", "-e", &rehearsed]);
}

#[test]
fn a_branch_the_rehearsal_deletes_is_deleted() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "-D", "feature"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert!(
        !fixture.refs().contains_key("refs/heads/feature"),
        "{:?}",
        fixture.refs()
    );
}

#[test]
fn a_branch_the_rehearsal_creates_is_created() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "spike", "feature"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        fixture.git(&["rev-parse", "spike"]),
        fixture.git(&["rev-parse", "feature"])
    );
}

#[test]
fn a_branch_that_appeared_meanwhile_is_not_overwritten() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["branch", "spike", "feature"]);
    // The same name, created in the real repository in the meantime.
    fixture.git(&["branch", "spike", "main"]);
    let theirs = fixture.git(&["rev-parse", "spike"]);

    let error = apply::run(&sandbox, NOW).expect_err("refused");

    assert!(
        matches!(error, Error::Git { .. }),
        "git's own transaction refuses it: {error:?}"
    );
    assert_eq!(
        fixture.git(&["rev-parse", "spike"]),
        theirs,
        "their branch is untouched"
    );
}

#[test]
fn nothing_is_applied_when_any_part_of_the_transaction_would_fail() {
    let fixture = Fixture::new();
    // Two branches move: one is fine, one is not.
    fixture.git(&["branch", "second", "feature"]);
    let sandbox = rehearse(&fixture, &["branch", "-f", "second", "main"]);
    let worktree = sandbox.worktree();
    fixture.git_in(&worktree, &["branch", "-f", "feature", "main"]);
    // `feature` moves in the real repository after the rehearsal.
    fixture.git(&["branch", "-f", "feature", "main~1"]);
    let before = fixture.refs();

    apply::run(&sandbox, NOW).expect_err("refused");

    assert_eq!(
        fixture.refs(),
        before,
        "the good half of the batch must not land on its own"
    );
}

#[test]
fn a_rehearsal_that_moved_nothing_says_so_instead_of_pretending() {
    let fixture = Fixture::new();
    let sandbox = rehearse(&fixture, &["merge", "main"]);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("moved no refs"), "{message}");
    assert!(message.contains("discard"), "{message}");
}

#[test]
fn applying_twice_is_refused_rather_than_repeated() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    apply::run(&sandbox, NOW).expect("the first apply succeeds");
    let after_first = fixture.refs();
    let message = refusal(apply::run(&sandbox, NOW).expect_err("the second is refused"));

    // The repository has changed — by the first apply. The check does not care
    // who moved the ref, which is exactly right.
    assert!(message.contains("has changed since rehearsal"), "{message}");
    assert_eq!(fixture.refs(), after_first);
}

#[test]
fn an_untracked_file_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "collision-feature", "main"]);
    fixture.commit_file("collision.txt", "feature content\n", "add collision");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "collision-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        fixture.refs(),
        before_refs,
        "no refs or fetch anchors moved"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
    assert!(!undo.exists(), "a refused apply has no undo record");
}

#[test]
fn identical_contents_do_not_allow_an_untracked_file_to_become_tracked() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "identical-feature", "main"]);
    fixture.commit_file("collision.txt", "same contents\n", "add identical file");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "same contents\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "identical-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);
    std::fs::write(&undo, "previous undo record\n").expect("existing undo record");

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        fixture.refs(),
        before_refs,
        "refs and fetch anchors are unchanged"
    );
    assert_eq!(fixture.git(&["ls-files", "--", "collision.txt"]), "");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "same contents\n"
    );
    assert_eq!(
        std::fs::read_to_string(undo).expect("undo"),
        "previous undo record\n"
    );
}

#[test]
fn a_non_colliding_untracked_file_survives_apply() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "feature-file", "main"]);
    fixture.commit_file("tracked-by-rehearsal.txt", "feature content\n", "add file");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("scratch.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature-file"]);

    apply::run(&sandbox, NOW).expect("apply succeeds");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("scratch.txt")).expect("untracked file"),
        "user content\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("tracked-by-rehearsal.txt"))
            .expect("rehearsed file"),
        "feature content\n"
    );
}

#[cfg(unix)]
#[test]
fn identical_targets_do_not_allow_an_untracked_symlink_to_become_tracked() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "identical-link-feature", "main"]);
    let path = fixture.repo().join("collision-link");
    std::os::unix::fs::symlink("file.txt", &path).expect("target symlink");
    fixture.git(&["add", "collision-link"]);
    fixture.git(&["commit", "-m", "add identical symlink"]);
    fixture.git(&["checkout", "-q", "main"]);
    std::os::unix::fs::symlink("file.txt", &path).expect("untracked symlink");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "identical-link-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_link(path).expect("symlink"),
        std::path::Path::new("file.txt")
    );
    assert_eq!(fixture.git(&["ls-files", "--", "collision-link"]), "");
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn identical_contents_do_not_allow_a_case_equivalent_untracked_file_to_become_tracked() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "identical-case-feature", "main"]);
    fixture.commit_file(
        "Collision.txt",
        "same contents\n",
        "add case-equivalent file",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "same contents\n");
    if !fixture.repo().join("Collision.txt").exists() {
        return; // No filesystem alias on a case-sensitive volume.
    }
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "identical-case-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "same contents\n"
    );
}

#[test]
fn an_untracked_nested_repository_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "nested-feature", "main"]);
    fixture.commit_file(
        "vendor/important.txt",
        "feature content\n",
        "add nested file",
    );
    fixture.git(&["checkout", "-q", "main"]);

    let vendor = fixture.repo().join("vendor");
    std::fs::create_dir_all(&vendor).expect("nested repository directory");
    fixture.git_in(&vendor, &["init", "-q"]);
    fixture.git_in(&vendor, &["config", "user.name", "Fixture"]);
    fixture.git_in(
        &vendor,
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(vendor.join("important.txt"), "user content\n").expect("untracked nested file");

    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "nested-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(vendor.join("important.txt")).expect("nested file"),
        "user content\n"
    );
    assert!(!undo.exists(), "a refused apply has no undo record");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn distinct_non_utf8_untracked_names_do_not_collide() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "non-utf8-feature", "main"]);
    let tracked = OsString::from_vec(b"tracked-\x80.txt".to_vec());
    std::fs::write(fixture.repo().join(&tracked), "feature content\n").expect("tracked file");
    let status = Command::new("git")
        .arg("-C")
        .arg(fixture.repo())
        .args(["add", "--"])
        .arg(&tracked)
        .status()
        .expect("git add runs");
    assert!(status.success(), "git add failed");
    fixture.git(&["commit", "-m", "add non-UTF-8 file"]);
    fixture.git(&["checkout", "-q", "main"]);

    let untracked = OsString::from_vec(b"tracked-\x81.txt".to_vec());
    std::fs::write(fixture.repo().join(&untracked), "user content\n").expect("untracked file");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "non-utf8-feature"]);

    apply::run(&sandbox, NOW).expect("distinct names do not collide");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join(&untracked)).expect("untracked file"),
        "user content\n"
    );
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn case_equivalent_untracked_names_are_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "case-feature", "main"]);
    fixture.commit_file("Collision.txt", "feature content\n", "add collision");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "case-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
}

#[test]
fn an_ignored_untracked_file_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.commit_file(".gitignore", "ignored.txt\n", "ignore a generated file");
    fixture.git(&["checkout", "-q", "-b", "ignored-feature", "main"]);
    fixture.write("ignored.txt", "feature content\n");
    fixture.git(&["add", "-f", "ignored.txt"]);
    fixture.git(&["commit", "-m", "add ignored file"]);
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("ignored.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "ignored-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("ignored.txt")).expect("ignored file"),
        "user content\n"
    );
}

#[test]
fn an_untracked_gitignore_that_would_become_tracked_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "gitignore-feature", "main"]);
    fixture.commit_file(".gitignore", "ignored.txt\n", "add ignore rules");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write(".gitignore", "user-pattern\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "gitignore-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join(".gitignore")).expect("ignore file"),
        "user-pattern\n"
    );
}

#[test]
fn an_empty_untracked_directory_that_would_become_a_file_is_not_replaced() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "empty-dir-feature", "main"]);
    fixture.commit_file("empty-dir", "feature content\n", "replace empty directory");
    fixture.git(&["checkout", "-q", "main"]);
    std::fs::create_dir(fixture.repo().join("empty-dir")).expect("empty untracked directory");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "empty-dir-feature"]);
    let before_refs = fixture.refs();

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(fixture.refs(), before_refs, "nothing moved");
    assert!(fixture.repo().join("empty-dir").is_dir());
}

#[cfg(unix)]
#[test]
fn an_empty_untracked_directory_is_not_replaced_by_a_symlink_to_a_directory() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "directory-link-feature", "main"]);
    let path = fixture.repo().join("empty-dir");
    std::os::unix::fs::symlink(".", &path).expect("target symlink");
    fixture.git(&["add", "empty-dir"]);
    fixture.git(&["commit", "-m", "add directory symlink"]);
    fixture.git(&["checkout", "-q", "main"]);
    std::fs::create_dir(&path).expect("empty untracked directory");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "directory-link-feature"]);
    let before_refs = fixture.refs();
    let undo = std::path::PathBuf::from(fixture.git(&["rev-parse", "--absolute-git-dir"]))
        .join(git_rehearse::undo::UNDO_FILE);
    std::fs::write(&undo, "previous undo record\n").expect("existing undo record");

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        fixture.refs(),
        before_refs,
        "refs and fetch anchors are unchanged"
    );
    let metadata = std::fs::symlink_metadata(&path).expect("original directory");
    assert!(
        metadata.file_type().is_dir(),
        "must remain a directory, not a symlink"
    );
    assert_eq!(std::fs::read_dir(path).expect("directory").count(), 0);
    assert_eq!(
        std::fs::read_to_string(undo).expect("undo"),
        "previous undo record\n"
    );
}

#[cfg(unix)]
#[test]
fn collision_probe_ignores_command_scoped_hooks_and_excludes_config() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "config-feature", "main"]);
    fixture.commit_file("collision.txt", "feature content\n", "add collision");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("collision.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "config-feature"]);

    let hooks = fixture.scratch("command-hooks");
    let marker = fixture.base().join("command-hook-ran");
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nprintf hook > '{}'\n", marker.display()),
    )
    .expect("hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("hook metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).expect("hook executable");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", &hooks)
        .env("GIT_CONFIG_KEY_1", "core.excludesFile")
        .env("GIT_CONFIG_VALUE_1", fixture.repo().join("collision.txt"))
        .args(["apply", sandbox.id()])
        .output()
        .expect("apply runs");

    assert!(
        !output.status.success(),
        "stdout={} stderr={} file={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read(fixture.repo().join("collision.txt"))
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("untracked"));
    assert!(!marker.exists(), "probe ran a command-scoped hook");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
}

#[test]
fn sparse_checkout_preserves_an_untracked_path_outside_the_sparse_cone() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "sparse-feature", "main"]);
    fixture.commit_file(
        "outside/tracked.txt",
        "feature content\n",
        "add outside sparse cone",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.git(&["sparse-checkout", "init", "--cone"]);
    fixture.git(&["sparse-checkout", "set", "--skip-checks", "file.txt"]);
    fixture.write("outside/untracked.txt", "user content\n");
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "sparse-feature"]);

    apply::run(&sandbox, NOW).expect("sparse-excluded untracked path is preserved");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("outside/untracked.txt"))
            .expect("untracked file"),
        "user content\n"
    );
}

#[test]
fn collision_probe_supports_split_indexes() {
    let fixture = Fixture::new();
    fixture.git(&["config", "core.splitIndex", "true"]);
    fixture.git(&["update-index", "--split-index"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "feature"]);

    apply::run(&sandbox, NOW).expect("split index is supported");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("carried file"),
        "three\n"
    );
}

#[test]
fn a_sparse_reset_leaves_an_identical_untracked_file_at_a_skipped_target_path_alone() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "sparse-identical-feature", "main"]);
    fixture.commit_file(
        "outside/collision.txt",
        "same contents\n",
        "add excluded file",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.git(&["sparse-checkout", "init", "--cone"]);
    fixture.git(&["sparse-checkout", "set", "--skip-checks", "inside"]);
    fixture.write("outside/collision.txt", "same contents\n");
    let sandbox = rehearse(
        &fixture,
        &["merge", "--no-edit", "sparse-identical-feature"],
    );

    apply::run(&sandbox, NOW).expect("a skipped path is not checked out");

    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("outside/collision.txt"))
            .expect("original file"),
        "same contents\n"
    );
}

#[test]
fn a_carried_result_cannot_hide_a_collision_during_the_intermediate_reset() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "carried-feature", "main"]);
    fixture.commit_file(
        "collision.txt",
        "feature content\n",
        "add collision during reset",
    );
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("file.txt", "carried work\n");
    fixture.write("collision.txt", "user content\n");
    let mut sandbox = rehearse(&fixture, &["merge", "--no-edit", "carried-feature"]);

    // Simulate a carried conflict resolution that removes the path introduced
    // by the branch. The real apply still resets through that branch first.
    fixture.git_in(&sandbox.worktree(), &["rm", "-q", "collision.txt"]);
    let result = fixture.git_in(&sandbox.worktree(), &["stash", "create"]);
    fixture.git_in(
        &sandbox.worktree(),
        &["update-ref", "refs/rehearse/replayed", &result],
    );
    sandbox
        .record_replay(carry::Replay::Restored {
            result: Some(result),
        })
        .expect("metadata is writable");

    let message = refusal(apply::run(&sandbox, NOW).expect_err("refused"));

    assert!(message.contains("untracked"), "{message}");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("collision.txt")).expect("untracked file"),
        "user content\n"
    );
}

#[cfg(unix)]
#[test]
fn collision_preflight_does_not_run_inherited_template_hooks() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "hook-feature", "main"]);
    fixture.commit_file("new.txt", "feature content\n", "add file");
    fixture.git(&["checkout", "-q", "main"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "hook-feature"]);

    let template = fixture.scratch("hostile-template");
    let hooks = template.join("hooks");
    std::fs::create_dir_all(&hooks).expect("template hooks directory");
    let marker = fixture.base().join("template-hook-ran");
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nprintf hook > '{}'\n", marker.display()),
    )
    .expect("template hook");
    let mut permissions = std::fs::metadata(&hook)
        .expect("hook metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).expect("hook executable");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .env("GIT_TEMPLATE_DIR", &template)
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .args(["apply", sandbox.id()])
        .output()
        .expect("git-rehearse apply runs");
    assert!(
        output.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!marker.exists(), "the inherited template hook ran");
}

#[test]
fn collision_preflight_resolves_a_separate_git_directory() {
    let fixture = Fixture::new();
    let separate = fixture.scratch("separate-git");
    let git_dir = fixture.repo().join(".git");
    std::fs::rename(&git_dir, &separate).expect("move git directory");
    std::fs::write(&git_dir, format!("gitdir: {}\n", separate.display())).expect("gitdir file");
    fixture.git(&["checkout", "-q", "-b", "separate-feature", "main"]);
    fixture.commit_file("new.txt", "feature content\n", "add file");
    fixture.git(&["checkout", "-q", "main"]);
    let sandbox = rehearse(&fixture, &["merge", "--no-edit", "separate-feature"]);

    apply::run(&sandbox, NOW).expect("separate git directory is supported");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("new.txt")).expect("rehearsed file"),
        "feature content\n"
    );
}

#[test]
fn collision_preflight_matches_a_sha256_repository() {
    let fixture = Fixture::new();
    let repo = fixture.scratch("sha256-repo");
    fixture.git_in(
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    fixture.git_in(&repo, &["config", "user.name", "Fixture"]);
    fixture.git_in(&repo, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(repo.join("base.txt"), "base\n").expect("base file");
    fixture.git_in(&repo, &["add", "base.txt"]);
    fixture.git_in(&repo, &["commit", "-q", "-m", "base"]);
    fixture.git_in(&repo, &["checkout", "-q", "-b", "sha256-feature"]);
    std::fs::write(repo.join("new.txt"), "feature content\n").expect("feature file");
    fixture.git_in(&repo, &["add", "new.txt"]);
    fixture.git_in(&repo, &["commit", "-q", "-m", "feature"]);
    fixture.git_in(&repo, &["checkout", "-q", "main"]);

    let plan = preflight::run(&repo)
        .expect("sha256 repository passes preflight")
        .into_plan(vec![
            "merge".to_owned(),
            "--no-edit".to_owned(),
            "sha256-feature".to_owned(),
        ]);
    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    execute::run(&sandbox.worktree(), &plan.command, None).expect("the command runs");

    apply::run(&sandbox, NOW).expect("sha256 repository applies");
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).expect("rehearsed file"),
        "feature content\n"
    );
}
