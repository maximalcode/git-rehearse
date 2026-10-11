//! Public CLI ownership and no-effect acceptance for reviewed-result Apply.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::Fixture;

fn json_run(fixture: &Fixture, args: &[&str], expected_exit: i32) -> Value {
    let (exit, stdout, stderr) = fixture.rehearse(args);
    assert_eq!(exit, expected_exit, "{stdout}\n{stderr}");
    serde_json::from_str(&stdout).expect("one public JSON document")
}

fn clean_merge(fixture: &Fixture, branch: &str) -> Value {
    json_run(
        fixture,
        &["--json", "--keep", "merge", "--no-edit", branch],
        0,
    )
}

fn field<'a>(document: &'a Value, name: &str) -> &'a str {
    document[name].as_str().expect("public report field")
}

#[derive(Debug, PartialEq, Eq)]
struct ProtectedState {
    refs: BTreeMap<String, String>,
    head: Vec<u8>,
    index: Vec<u8>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    journal: Option<Vec<u8>>,
    undo: Option<Vec<u8>>,
    retained_metadata: Vec<u8>,
}

fn worktree_files(root: &Path, at: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in std::fs::read_dir(at).expect("fixture directory") {
        let entry = entry.expect("directory entry");
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type().expect("file type").is_dir() {
            worktree_files(root, &path, files);
        } else {
            files.insert(
                path.strip_prefix(root).expect("relative path").to_owned(),
                std::fs::read(&path).expect("fixture bytes"),
            );
        }
    }
}

fn protected_state(fixture: &Fixture, report: &Value) -> ProtectedState {
    let mut files = BTreeMap::new();
    worktree_files(fixture.repo(), fixture.repo(), &mut files);
    let git_dir = fixture.repo().join(".git");
    let sandbox = Path::new(field(report, "sandbox"));
    ProtectedState {
        refs: fixture.refs(),
        head: std::fs::read(git_dir.join("HEAD")).expect("origin HEAD"),
        index: std::fs::read(git_dir.join("index")).expect("origin index"),
        files,
        journal: std::fs::read(git_dir.join("rehearse-apply")).ok(),
        undo: std::fs::read(git_dir.join("rehearse-undo")).ok(),
        retained_metadata: std::fs::read(
            sandbox.parent().expect("retained root").join("meta.json"),
        )
        .expect("retained metadata"),
    }
}

struct PausedApply {
    child: Option<Child>,
    marker: PathBuf,
}

impl PausedApply {
    fn start(fixture: &Fixture, report: &Value, stage: &str) -> Self {
        let marker = fixture.base().join("conditional-apply-paused");
        let child = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
            .current_dir(fixture.repo())
            .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
            .env("GIT_EDITOR", "true")
            .env(
                "GIT_REHEARSE_PAUSE_APPLY_AT",
                format!("{stage}={}", marker.display()),
            )
            .args([
                "--json",
                "apply",
                field(report, "id"),
                "--expected-result-revision",
                field(report, "result_revision"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("conditional Apply process");
        let mut paused = Self {
            child: Some(child),
            marker,
        };
        let deadline = Instant::now() + Duration::from_mins(1);
        while !paused.marker.exists() && Instant::now() < deadline {
            assert!(
                paused
                    .child
                    .as_mut()
                    .expect("child")
                    .try_wait()
                    .expect("child status")
                    .is_none(),
                "Apply exited before its coordination point"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            paused.marker.exists(),
            "Apply reached its coordination point"
        );
        paused
    }

    fn finish(mut self) -> Output {
        std::fs::remove_file(&self.marker).expect("resume Apply");
        self.child
            .take()
            .expect("child")
            .wait_with_output()
            .expect("Apply exits")
    }
}

impl Drop for PausedApply {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn assert_refused_without_effects(
    fixture: &Fixture,
    report: &Value,
    before: &ProtectedState,
    apply: PausedApply,
) {
    let output = apply.finish();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let refusal: Value = serde_json::from_slice(&output.stdout).expect("refusal JSON");
    assert_eq!(refusal["kind"], "refused");
    assert!(field(&refusal, "message").contains("refresh"), "{refusal}");
    assert_eq!(&protected_state(fixture, report), before);
    assert!(
        Path::new(field(report, "sandbox")).is_dir(),
        "retained result remains"
    );
}

#[test]
fn another_process_moving_a_result_ref_invalidates_pending_conditional_apply() {
    let fixture = Fixture::new();
    fixture.write("untracked.txt", "keep this unrelated file\n");
    let report = clean_merge(&fixture, "feature");
    let apply = PausedApply::start(&fixture, &report, "before-claim");
    let sandbox = Path::new(field(&report, "sandbox"));
    let original = report["pre_state"]["refs/heads/main"]
        .as_str()
        .expect("original ref");
    fixture.git_in(sandbox, &["update-ref", "refs/heads/main", original]);
    let before = protected_state(&fixture, &report);
    assert_refused_without_effects(&fixture, &report, &before, apply);
}

#[test]
fn trailing_conditional_option_refuses_pending_continue_without_effects() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "followup", "feature"]);
    fixture.commit_file("later.txt", "later content\n", "later result");
    fixture.git(&["checkout", "-q", "main"]);
    let report = clean_merge(&fixture, "feature");
    let sandbox = Path::new(field(&report, "sandbox"));
    fixture.git_in(sandbox, &["merge", "--no-ff", "--no-commit", "followup"]);

    let before = protected_state(&fixture, &report);
    let refusal = json_run(
        &fixture,
        &[
            "--json",
            "--apply",
            "continue",
            field(&report, "id"),
            "--expected-result-revision=bad",
        ],
        4,
    );
    assert_eq!(refusal["kind"], "refused");
    assert_eq!(protected_state(&fixture, &report), before);
    assert!(sandbox.is_dir(), "retained pending result remains");
}

#[test]
fn another_process_moving_only_a_carried_object_invalidates_pending_apply() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "carried-feature", "main"]);
    fixture.commit_file("new.txt", "feature\n", "new file");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.write("file.txt", "staged local change\n");
    fixture.git(&["add", "file.txt"]);
    fixture.write("file.txt", "unstaged local change\n");
    fixture.write("untracked.txt", "untracked stays excluded\n");
    let report = clean_merge(&fixture, "carried-feature");
    let before = protected_state(&fixture, &report);
    let apply = PausedApply::start(&fixture, &report, "before-claim");
    let sandbox = Path::new(field(&report, "sandbox"));
    let snapshot = fixture.git_in(sandbox, &["rev-parse", "refs/rehearse/carried"]);
    fixture.git_in(
        sandbox,
        &["update-ref", "refs/rehearse/replayed", &snapshot],
    );
    assert_eq!(
        protected_state(&fixture, &report),
        before,
        "carry metadata stayed identical"
    );
    assert_refused_without_effects(&fixture, &report, &before, apply);
}

#[test]
fn a_later_continue_cannot_replace_a_result_already_named_by_apply() {
    let fixture = Fixture::new();
    fixture.git(&["checkout", "-q", "-b", "followup", "feature"]);
    fixture.commit_file("later.txt", "later content\n", "later result");
    fixture.git(&["checkout", "-q", "main"]);
    let report = clean_merge(&fixture, "feature");
    let apply = PausedApply::start(&fixture, &report, "before-claim");
    let sandbox = Path::new(field(&report, "sandbox"));
    fixture.git_in(sandbox, &["merge", "--no-ff", "--no-commit", "followup"]);
    let continued = json_run(
        &fixture,
        &["--json", "--keep", "continue", field(&report, "id")],
        0,
    );
    assert_ne!(continued["result_revision"], report["result_revision"]);
    let before = protected_state(&fixture, &continued);
    assert_refused_without_effects(&fixture, &continued, &before, apply);
    let applied = json_run(
        &fixture,
        &[
            "--json",
            "apply",
            field(&continued, "id"),
            "--expected-result-revision",
            field(&continued, "result_revision"),
        ],
        0,
    );
    assert_eq!(applied["result_revision"], continued["result_revision"]);
    assert_eq!(applied["result_endpoints"], continued["result_endpoints"]);
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("later.txt")).expect("adopted file"),
        "later content\n"
    );
}

#[test]
fn conditional_apply_holds_ownership_through_the_ref_transaction() {
    let fixture = Fixture::new();
    let report = clean_merge(&fixture, "feature");
    let apply = PausedApply::start(&fixture, &report, "before-ref-transaction");
    for command in ["continue", "discard"] {
        let refusal = json_run(&fixture, &["--json", command, field(&report, "id")], 4);
        let message = field(&refusal, "message");
        assert!(
            message.contains("active") || message.contains("another process owns"),
            "{refusal}"
        );
    }
    let listing = json_run(&fixture, &["--json", "list"], 0);
    assert!(listing["rehearsals"][0]["result_revision"].is_null());
    let output = apply.finish();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let applied: Value = serde_json::from_slice(&output.stdout).expect("apply JSON");
    assert_eq!(applied["result_revision"], report["result_revision"]);
    assert_eq!(
        fixture.git(&["rev-parse", "HEAD"]),
        report["result_endpoints"]["results"]["HEAD"]
            .as_str()
            .expect("reviewed HEAD")
    );
}
