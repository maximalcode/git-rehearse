//! Scripted git fixtures for the integration tests.
//!
//! Repositories are built by running real `git`, never by checking in a `.git`
//! directory: a committed fixture repo is a binary blob nobody can review, and
//! it rots against the git version that reads it.
//!
//! Every fixture invocation is hermetic — no global or system config, a fixed
//! identity and a fixed date — so the tests behave the same on a contributor's
//! machine as in CI. The code under test is deliberately *not* hermetic: it
//! runs the user's real git with the user's real config, which is design
//! principle 1.

// Every integration-test binary compiles this module separately and uses the
// part of it that its subject needs; the unused remainder is not dead code,
// it is another test file's scaffolding.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use git_rehearse::sandbox::{Checkout, Plan};
use tempfile::TempDir;

/// A temporary directory holding a real git repository and a cache root.
pub struct Fixture {
    // Kept alive for its Drop: the whole tree goes when the test ends, panic
    // or not.
    _dir: TempDir,
    base: PathBuf,
    repo: PathBuf,
    cache: PathBuf,
    /// An empty directory, used as git's template and hooks path so nothing
    /// the developer has configured globally can reach a fixture.
    empty: PathBuf,
}

impl Fixture {
    /// A repository with `main` (two commits, tagged `v1`) and `feature` (one
    /// more commit), checked out on `main`.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        // Canonicalised because macOS hands out /var/... paths that are really
        // /private/var/..., and the cache directory name is derived from the
        // canonical path.
        let base = dir
            .path()
            .canonicalize()
            .expect("the temporary directory resolves");
        let fixture = Self {
            _dir: dir,
            repo: base.join("repo"),
            cache: base.join("cache"),
            empty: base.join("empty"),
            base,
        };
        std::fs::create_dir_all(&fixture.repo).expect("repo directory");
        std::fs::create_dir_all(&fixture.empty).expect("empty directory");

        fixture.git(&["init", "-b", "main"]);
        fixture.commit("one", "one\n");
        fixture.commit("two", "two\n");
        fixture.git(&["tag", "v1"]);
        fixture.git(&["checkout", "-b", "feature"]);
        fixture.commit("three", "three\n");
        fixture.git(&["checkout", "main"]);
        fixture
    }

    /// The real repository's worktree root, canonicalised.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// A cache root for [`git_rehearse::sandbox`] — never the user's own.
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// Runs git inside the fixture repository, panicking with git's stderr.
    pub fn git(&self, args: &[&str]) -> String {
        run_git(&self.repo, &self.empty, args)
    }

    /// Runs git inside some other directory — usually a sandbox worktree.
    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        run_git(dir, &self.empty, args)
    }

    /// The temporary directory the repository and cache live in — where sibling
    /// repositories, clones and linked worktrees get created.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Commits `content` as `file.txt` with the given message.
    pub fn commit(&self, message: &str, content: &str) {
        self.commit_file("file.txt", content, message);
    }

    /// Commits `content` as `name` with the given message.
    pub fn commit_file(&self, name: &str, content: &str, message: &str) {
        self.write(name, content);
        self.git(&["add", "--", name]);
        self.git(&["commit", "-m", message]);
    }

    /// Writes a file in the worktree without committing it, creating parent
    /// directories so `"sub/file.txt"` works.
    pub fn write(&self, name: &str, content: &str) {
        let path = self.repo.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directory");
        }
        std::fs::write(path, content).expect("write into the worktree");
    }

    /// A second repository beside the first, with one commit. Used as a
    /// submodule source and as somewhere to clone from.
    pub fn sibling(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        std::fs::create_dir_all(&path).expect("sibling directory");
        self.git_in(&path, &["init", "-b", "main"]);
        std::fs::write(path.join("file.txt"), "sibling\n").expect("write sibling file");
        self.git_in(&path, &["add", "file.txt"]);
        self.git_in(&path, &["commit", "-m", "sibling"]);
        path
    }

    /// An empty directory beside the repository, created for the caller.
    pub fn scratch(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        std::fs::create_dir_all(&path).expect("scratch directory");
        path
    }

    /// Every ref in the repository, `refname -> sha`. This is the shape
    /// preflight will hand to [`Plan::pre_state`], and what an apply compares
    /// against to prove nothing moved.
    pub fn refs(&self) -> BTreeMap<String, String> {
        refs_of(self, &self.repo)
    }

    /// Every ref in some other repository — used to compare a sandbox with the
    /// real thing.
    pub fn refs_in(&self, dir: &Path) -> BTreeMap<String, String> {
        refs_of(self, dir)
    }

    /// A plan against this fixture, with a truthful pre-state.
    pub fn plan(&self, command: &[&str], checkout: Checkout) -> Plan {
        Plan {
            repo: self.repo.clone(),
            command: command.iter().map(|arg| (*arg).to_owned()).collect(),
            checkout,
            pre_state: self.refs(),
        }
    }
}

fn refs_of(fixture: &Fixture, dir: &Path) -> BTreeMap<String, String> {
    fixture
        .git_in(dir, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, sha)| (name.to_owned(), sha.to_owned()))
        .collect()
}

fn run_git(dir: &Path, empty: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        // Signing and hooks are the developer's business, not the fixture's.
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg(format!("core.hooksPath={}", empty.display()))
        // git 2.38 blocked file:// submodules by default (CVE-2022-39253);
        // fixtures have to opt back in to build a submodule locally.
        .arg("-c")
        .arg("protocol.file.allow=always")
        .args(args)
        // Pointing config at files that do not exist is git's supported way of
        // saying "no config here".
        .env("GIT_CONFIG_GLOBAL", empty.join("no-such-gitconfig"))
        .env("GIT_CONFIG_SYSTEM", empty.join("no-such-gitconfig"))
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed in {}:\n{}",
        args.join(" "),
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}
