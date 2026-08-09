//! Preflight against real repositories: what it records, and what it refuses.
//!
//! The refusal tests assert on the *message*, not just on the failure. These
//! messages are what a user meets at the worst possible moment, and a refusal
//! that does not say what to do next is a bug in the product, not a detail of
//! the implementation.

mod support;

use git_rehearse::sandbox::{self, Checkout};
use git_rehearse::{Error, preflight};
use support::Fixture;

/// Unwraps a refusal, insisting it is one: an internal error dressed as a
/// refusal (or the reverse) would exit with the wrong code, and exit codes are
/// API from v0.1 on.
fn refusal(error: Error) -> String {
    match error {
        Error::Refused(message) => message,
        other => panic!("expected a refusal, got: {other:?}"),
    }
}

#[test]
fn a_clean_repository_passes_and_records_where_everything_is() {
    let fixture = Fixture::new();

    let preflight = preflight::run(fixture.repo()).expect("a clean repository passes");

    assert_eq!(preflight.repo, fixture.repo());
    assert_eq!(preflight.checkout, Checkout::Branch("main".to_owned()));

    let main = fixture.git(&["rev-parse", "main"]);
    let feature = fixture.git(&["rev-parse", "feature"]);
    assert_eq!(preflight.pre_state.get("refs/heads/main"), Some(&main));
    assert_eq!(
        preflight.pre_state.get("refs/heads/feature"),
        Some(&feature)
    );
    assert_eq!(preflight.pre_state.get(preflight::HEAD_KEY), Some(&main));
    // Branches and HEAD only: a tag appearing while the user reads the report
    // does not make the rehearsed result stale.
    assert!(
        !preflight.pre_state.contains_key("refs/tags/v1"),
        "{:?}",
        preflight.pre_state
    );
}

#[test]
fn preflight_finds_the_repository_from_a_subdirectory() {
    let fixture = Fixture::new();
    fixture.commit_file("sub/file.txt", "nested\n", "nested");

    let preflight = preflight::run(&fixture.repo().join("sub")).expect("finds the worktree root");

    assert_eq!(
        preflight.repo,
        fixture.repo(),
        "the root, not the subdirectory"
    );
}

#[test]
fn a_detached_head_is_recorded_as_detached() {
    let fixture = Fixture::new();
    let sha = fixture.git(&["rev-parse", "HEAD~1"]);
    fixture.git(&["checkout", "--detach", &sha]);

    let preflight = preflight::run(fixture.repo()).expect("a detached HEAD is fine");

    assert_eq!(preflight.checkout, Checkout::Detached(sha.clone()));
    assert_eq!(preflight.pre_state.get(preflight::HEAD_KEY), Some(&sha));
}

#[test]
fn the_snapshot_becomes_the_plan_the_sandbox_is_built_from() {
    let fixture = Fixture::new();

    let preflight = preflight::run(fixture.repo()).expect("a clean repository passes");
    let expected = preflight.pre_state.clone();
    let plan = preflight.into_plan(vec!["rebase".to_owned(), "main".to_owned()]);
    let sandbox = sandbox::create(fixture.cache(), &plan, 1_786_248_000).expect("sandbox");

    // The pre-state travels from the real repo, through the plan, into the file
    // apply will verify against — untouched.
    assert_eq!(sandbox.meta().pre_state, expected);
    assert_eq!(sandbox.meta().repo_path, fixture.repo());
    assert_eq!(
        fixture.git_in(&sandbox.worktree(), &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
}

#[test]
fn uncommitted_changes_are_refused_and_the_files_are_named() {
    let fixture = Fixture::new();
    fixture.write("file.txt", "edited\n");

    let message = refusal(preflight::run(fixture.repo()).expect_err("dirty is refused"));

    assert!(message.contains("file.txt"), "{message}");
    assert!(message.contains("commit or stash"), "{message}");
    assert!(
        message.contains("v1.x"),
        "the user should know it is coming: {message}"
    );
}

#[test]
fn untracked_files_are_not_a_refusal() {
    let fixture = Fixture::new();
    fixture.write("scratch.txt", "not tracked\n");

    // `git rebase` runs happily with untracked files present. Refusing here
    // would be stricter than git, which is its own kind of surprise.
    preflight::run(fixture.repo()).expect("untracked files are not in the way");
}

#[test]
fn a_directory_that_is_not_a_repository_is_refused() {
    let fixture = Fixture::new();
    let elsewhere = fixture.scratch("not-a-repo");

    let message = refusal(preflight::run(&elsewhere).expect_err("not a repository"));

    assert!(message.contains("not a git repository"), "{message}");
    assert!(
        message.contains("Run git-rehearse from inside"),
        "{message}"
    );
}

#[test]
fn a_bare_repository_is_refused() {
    let fixture = Fixture::new();
    let bare = fixture.base().join("bare.git");
    fixture.git_in(fixture.base(), &["clone", "--bare", "repo", "bare.git"]);

    let message = refusal(preflight::run(&bare).expect_err("bare is refused"));

    assert!(message.contains("bare repository"), "{message}");
    assert!(message.contains("worktree"), "{message}");
}

#[test]
fn a_shallow_clone_is_refused() {
    let fixture = Fixture::new();
    let shallow = fixture.base().join("shallow");
    let url = format!("file://{}", fixture.repo().display());
    fixture.git_in(fixture.base(), &["clone", "--depth", "1", &url, "shallow"]);

    let message = refusal(preflight::run(&shallow).expect_err("shallow is refused"));

    assert!(message.contains("shallow"), "{message}");
    assert!(
        message.contains("--unshallow"),
        "the remedy is a command: {message}"
    );
}

#[test]
fn a_second_worktree_is_refused() {
    let fixture = Fixture::new();
    let linked = fixture.base().join("linked");
    fixture.git(&["worktree", "add", &linked.to_string_lossy(), "feature"]);

    let message = refusal(preflight::run(fixture.repo()).expect_err("two worktrees are refused"));

    assert!(message.contains("2 worktrees"), "{message}");
    assert!(message.contains("git worktree list"), "{message}");

    // And from inside the linked worktree too — the hazard is the same one.
    let from_linked =
        refusal(preflight::run(&linked).expect_err("also refused from the other side"));
    assert!(from_linked.contains("worktree"), "{from_linked}");
}

#[test]
fn submodules_are_refused() {
    let fixture = Fixture::new();
    let sibling = fixture.sibling("dependency");
    fixture.git(&[
        "submodule",
        "add",
        "--",
        &sibling.to_string_lossy(),
        "vendor",
    ]);
    fixture.git(&["commit", "-m", "add submodule"]);

    let message = refusal(preflight::run(fixture.repo()).expect_err("submodules are refused"));

    assert!(message.contains("submodules"), "{message}");
    assert!(
        message.contains("local clone does not bring them along"),
        "{message}"
    );
}

#[test]
fn git_lfs_is_refused() {
    let fixture = Fixture::new();
    fixture.commit_file(
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "track binaries with lfs",
    );

    let message = refusal(preflight::run(fixture.repo()).expect_err("LFS is refused"));

    assert!(message.contains("Git LFS"), "{message}");
    assert!(message.contains(".gitattributes"), "{message}");
}

#[test]
fn a_gitattributes_without_lfs_is_not_a_refusal() {
    let fixture = Fixture::new();
    fixture.commit_file(".gitattributes", "* text=auto\n", "normalise line endings");

    preflight::run(fixture.repo()).expect("plain attributes are not LFS");
}

#[test]
fn a_repository_with_no_commits_is_refused() {
    let fixture = Fixture::new();
    let fresh = fixture.scratch("fresh");
    fixture.git_in(&fresh, &["init", "-b", "main"]);

    let message = refusal(preflight::run(&fresh).expect_err("nothing to rehearse"));

    assert!(message.contains("no commits yet"), "{message}");
}

#[test]
fn structural_refusals_come_before_the_dirty_worktree_one() {
    let fixture = Fixture::new();
    let sibling = fixture.sibling("dependency");
    fixture.git(&[
        "submodule",
        "add",
        "--",
        &sibling.to_string_lossy(),
        "vendor",
    ]);
    fixture.git(&["commit", "-m", "add submodule"]);
    fixture.write("file.txt", "edited\n");

    let message = refusal(preflight::run(fixture.repo()).expect_err("refused"));

    // Being told to commit your changes, and only then being told submodules
    // are unsupported anyway, wastes someone's afternoon.
    assert!(message.contains("submodules"), "{message}");
}
