//! Lifecycle tests for the shadow clone, against real git repositories.
//!
//! These are the tests that hold design principle 3 in place: a sandbox is
//! inert (no remotes, no hooks), complete (every branch and tag the real repo
//! had), isolated (nothing it does reaches the real repository), and
//! disposable (discard now, prune later).

mod support;

use git_rehearse::sandbox::{self, Checkout, DEFAULT_TTL_SECS, Status};
use support::Fixture;

const NOW: u64 = 1_786_248_000;

#[test]
fn a_fresh_sandbox_is_inert_and_checked_out() {
    let fixture = Fixture::new();
    let plan = fixture.plan(
        &["rebase", "-i", "main"],
        Checkout::Branch("main".to_owned()),
    );

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    let worktree = sandbox.worktree();

    assert_eq!(
        fixture.git_in(&worktree, &["remote"]),
        "",
        "a sandbox with a remote is a sandbox an accidental push can escape"
    );

    let hooks_path = fixture.git_in(&worktree, &["config", "--local", "--get", "core.hooksPath"]);
    let hooks = std::path::Path::new(&hooks_path);
    assert!(
        hooks.starts_with(sandbox.root()),
        "hooks must point inside the rehearsal we own, not at {hooks_path}"
    );
    assert_eq!(
        std::fs::read_dir(hooks)
            .expect("hooks directory exists")
            .count(),
        0,
        "the hooks directory must stay empty"
    );

    assert_eq!(
        std::fs::read_to_string(worktree.join("file.txt")).expect("worktree is populated"),
        "two\n"
    );
    assert_eq!(
        fixture.git_in(&worktree, &["status", "--porcelain"]),
        "",
        "the checkout must be clean before the rehearsed command runs"
    );
    assert_eq!(
        fixture.git_in(&worktree, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
}

#[test]
fn the_sandbox_carries_the_repositorys_own_commit_identity() {
    let fixture = Fixture::new();
    // A per-repository identity: a work checkout, a second account, or this
    // project's own CLAUDE.md rule. `git clone` does not copy .git/config, so
    // without carrying it the rehearsal would commit as somebody else — or,
    // where there is no global identity at all, refuse to commit.
    fixture.git(&["config", "user.name", "Repo Local"]);
    fixture.git(&["config", "user.email", "repo-local@example.invalid"]);
    let plan = fixture.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");

    assert_eq!(
        fixture.git_in(
            &sandbox.worktree(),
            &["config", "--local", "--get", "user.name"]
        ),
        "Repo Local"
    );
    assert_eq!(
        fixture.git_in(
            &sandbox.worktree(),
            &["config", "--local", "--get", "user.email"]
        ),
        "repo-local@example.invalid"
    );
}

#[test]
fn the_sandbox_carries_the_repositorys_line_ending_policy() {
    let fixture = Fixture::new();
    // Same class as the identity: set locally, invisible to a clone, and it
    // decides the bytes the rehearsed command sees in the worktree. Without
    // it a conflict can present differently in the sandbox than it would at
    // home, which is the one thing the rehearsal must not do.
    fixture.git(&["config", "core.autocrlf", "input"]);
    let plan = fixture.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");

    assert_eq!(
        fixture.git_in(
            &sandbox.worktree(),
            &["config", "--local", "--get", "core.autocrlf"]
        ),
        "input"
    );
}

#[test]
fn the_meta_file_records_what_apply_will_need() {
    let fixture = Fixture::new();
    let pre_state = fixture.refs();
    let plan = fixture.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");

    let meta = sandbox.meta();
    assert_eq!(meta.command, vec!["merge", "feature"]);
    assert_eq!(meta.checkout, Checkout::Branch("main".to_owned()));
    assert_eq!(meta.repo_path, fixture.repo());
    assert_eq!(meta.created_unix, NOW);
    assert_eq!(meta.status, Status::Fresh);
    assert_eq!(
        meta.pre_state, pre_state,
        "the pre-state is evidence for the apply-time race check"
    );
    assert!(sandbox.root().join("meta.json").is_file());

    // And it survives the process: a later `git rehearse apply` reads it back.
    let listed = sandbox::list(fixture.cache(), None).expect("cache is listable");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].meta(), meta);
}

#[test]
fn every_branch_and_tag_survives_the_clone() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&["rebase", "main"], Checkout::Branch("feature".to_owned()));

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");

    let real = fixture.refs();
    let sandboxed = fixture.refs_in(&sandbox.worktree());
    // `git clone` makes a local branch only for HEAD and parks the rest under
    // refs/remotes/origin/*, which the remote takes with it when stripped.
    // Rehearsing `rebase main` from `feature` needs both to be local branches.
    for name in ["refs/heads/main", "refs/heads/feature", "refs/tags/v1"] {
        assert_eq!(
            sandboxed.get(name),
            real.get(name),
            "{name} must exist in the sandbox at the same commit"
        );
    }
    assert!(
        !sandboxed
            .keys()
            .any(|name| name.starts_with("refs/remotes/")),
        "no remote-tracking refs should be left: {sandboxed:?}"
    );
    assert_eq!(
        fixture.git_in(&sandbox.worktree(), &["symbolic-ref", "--short", "HEAD"]),
        "feature"
    );
}

#[test]
fn nothing_the_sandbox_does_reaches_the_real_repository() {
    let fixture = Fixture::new();
    let before = fixture.refs();
    let feature_sha = fixture.git(&["rev-parse", "feature"]);
    let plan = fixture.plan(&["rebase", "main"], Checkout::Branch("main".to_owned()));

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    let worktree = sandbox.worktree();

    // The worst a rehearsal could do: rewrite refs, delete a branch, then
    // garbage-collect the objects it shares with the real repo by hardlink.
    fixture.git_in(&worktree, &["branch", "-D", "feature"]);
    fixture.git_in(&worktree, &["reset", "--hard", "HEAD~1"]);
    std::fs::write(worktree.join("file.txt"), "rehearsed\n").expect("write in the sandbox");
    fixture.git_in(&worktree, &["commit", "-am", "rehearsed"]);
    fixture.git_in(&worktree, &["gc", "--prune=now", "--quiet"]);

    assert_eq!(
        fixture.refs(),
        before,
        "the real repository's refs must not move"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "two\n"
    );
    // Hardlinked objects are content-addressed and never rewritten in place;
    // the sandbox's gc unlinked its own copy and nothing else.
    fixture.git(&["cat-file", "-e", &feature_sha]);
}

#[test]
fn a_detached_head_is_reproduced_as_a_detached_head() {
    let fixture = Fixture::new();
    let sha = fixture.git(&["rev-parse", "HEAD~1"]);
    let plan = fixture.plan(&["cherry-pick", "feature"], Checkout::Detached(sha.clone()));

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");

    assert_eq!(
        fixture.git_in(&sandbox.worktree(), &["rev-parse", "HEAD"]),
        sha
    );
    assert_eq!(
        fixture.git_in(&sandbox.worktree(), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD",
        "HEAD must be detached, not on a branch"
    );
}

#[test]
fn a_failed_build_leaves_nothing_behind() {
    let fixture = Fixture::new();
    let plan = fixture.plan(
        &["rebase", "main"],
        Checkout::Branch("no-such-branch".to_owned()),
    );

    let error = sandbox::create(fixture.cache(), &plan, NOW).expect_err("checkout must fail");

    assert!(
        error.to_string().contains("no-such-branch"),
        "the refusal must say what git said: {error}"
    );
    assert!(
        sandbox::list(fixture.cache(), None)
            .expect("cache is listable")
            .is_empty(),
        "a half-built sandbox is worse than none"
    );
}

#[test]
fn rehearsals_started_in_the_same_second_do_not_collide() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));

    let first = sandbox::create(fixture.cache(), &plan, NOW).expect("first sandbox");
    let second = sandbox::create(fixture.cache(), &plan, NOW).expect("second sandbox");

    assert_ne!(first.id(), second.id());
    assert_ne!(first.root(), second.root());
    assert!(first.root().is_dir() && second.root().is_dir());
}

#[test]
fn discarding_removes_the_rehearsal_and_leaves_the_repository_alone() {
    let fixture = Fixture::new();
    let before = fixture.refs();
    let plan = fixture.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));

    let sandbox = sandbox::create(fixture.cache(), &plan, NOW).expect("sandbox is created");
    let root = sandbox.root().to_path_buf();
    sandbox.discard().expect("discard succeeds");

    assert!(!root.exists(), "discard is immediate, not deferred");
    assert_eq!(fixture.refs(), before);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("real worktree"),
        "two\n"
    );
}

#[test]
fn listing_can_be_narrowed_to_one_repository() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let plan = fixture.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));
    let other_plan = other.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));

    sandbox::create(fixture.cache(), &plan, NOW).expect("first");
    sandbox::create(fixture.cache(), &plan, NOW + 1).expect("second");
    // Both repositories share one cache root here, which is exactly the case
    // `git rehearse list` has to filter.
    sandbox::create(fixture.cache(), &other_plan, NOW).expect("other repository");

    let all = sandbox::list(fixture.cache(), None).expect("cache is listable");
    assert_eq!(all.len(), 3);
    assert!(
        all.windows(2)
            .all(|pair| pair[0].meta().created_unix <= pair[1].meta().created_unix),
        "listings are ordered oldest first"
    );

    let repo_id = all
        .iter()
        .find(|s| s.meta().repo_path == fixture.repo())
        .expect("the fixture repo is in the listing")
        .meta()
        .repo_id
        .clone();
    let mine = sandbox::list(fixture.cache(), Some(&repo_id)).expect("cache is listable");
    assert_eq!(mine.len(), 2);
    assert!(mine.iter().all(|s| s.meta().repo_path == fixture.repo()));
}

#[test]
fn prune_removes_what_has_expired_and_keeps_what_has_not() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&["merge", "feature"], Checkout::Branch("main".to_owned()));

    let old = sandbox::create(fixture.cache(), &plan, NOW).expect("old rehearsal");
    let fresh =
        sandbox::create(fixture.cache(), &plan, NOW + DEFAULT_TTL_SECS).expect("fresh rehearsal");

    let removed = sandbox::prune(
        fixture.cache(),
        NOW + DEFAULT_TTL_SECS + 1,
        DEFAULT_TTL_SECS,
    )
    .expect("prune runs");

    assert_eq!(removed, vec![old.id().to_owned()]);
    assert!(!old.root().exists());
    assert!(fresh.root().exists(), "a rehearsal inside its TTL is kept");
}

#[test]
fn prune_collects_a_directory_that_never_got_a_meta_file() {
    let fixture = Fixture::new();
    // What a run killed mid-clone leaves behind: no meta.json, so nothing but
    // the directory's own mtime says how old it is.
    let orphan = fixture
        .cache()
        .join("some-repo-0123456789abcdef")
        .join("1-00");
    std::fs::create_dir_all(orphan.join("sandbox")).expect("orphan directory");

    let removed =
        sandbox::prune(fixture.cache(), git_rehearse::now_unix() + 60, 0).expect("prune runs");

    assert_eq!(removed, vec!["1-00".to_owned()]);
    assert!(!orphan.exists());
    assert_eq!(
        sandbox::list(fixture.cache(), None)
            .expect("cache is listable")
            .len(),
        0
    );
}

#[test]
fn an_empty_cache_is_an_empty_listing_not_an_error() {
    let fixture = Fixture::new();
    let missing = fixture.cache().join("never-created");

    assert!(
        sandbox::list(&missing, None)
            .expect("no cache is fine")
            .is_empty()
    );
    assert_eq!(
        sandbox::prune(&missing, NOW, DEFAULT_TTL_SECS).expect("no cache is fine"),
        Vec::<String>::new()
    );
}
