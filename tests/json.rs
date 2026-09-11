//! `--json`, driven the way a program would drive it.
//!
//! These tests parse the process's real stdout rather than inspecting a
//! serialised struct, because the contract being tested is *"one document on
//! stdout and nothing else"* — and the way that broke in development was git
//! writing `Auto-merging …` to the same stream, which no amount of testing the
//! `Serialize` impl would have caught.

mod support;

use git_rehearse::sandbox::DEFAULT_TTL_SECS;
use support::Fixture;

const CLEAN: i32 = 0;
const STOPPED: i32 = 2;
const FAILED: i32 = 3;
const REFUSED: i32 = 4;

/// Parses stdout, insisting it is exactly one JSON document.
fn document(out: &str) -> serde_json::Value {
    serde_json::from_str(out)
        .unwrap_or_else(|e| panic!("stdout must be one JSON document and nothing else: {e}\n{out}"))
}

#[test]
fn a_clean_rehearsal_is_one_document_on_stdout() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");

    let (code, out, _) = fixture.rehearse(&["--json", "merge", "--no-edit", "feature"]);

    assert_eq!(code, CLEAN);
    let json = document(&out);
    assert_eq!(json["schema"], 1);
    assert_eq!(json["outcome"], "clean");
    assert_eq!(json["exit_code"], 0);
    assert_eq!(
        json["command"],
        serde_json::json!(["merge", "--no-edit", "feature"])
    );
    // Unattended and unclaimed: the same answer the text path gives.
    assert_eq!(json["decision"], "discarded");
}

#[test]
fn git_s_own_output_never_reaches_stdout() {
    // The regression: git writes "Auto-merging …" and "CONFLICT …" to its
    // stdout, and inheriting that put two lines of English in front of the
    // document — enough to break every caller that parses the stream.
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);

    let (code, out, err) = fixture.rehearse(&["--json", "rebase", "main"]);

    assert_eq!(code, STOPPED, "{err}");
    let json = document(&out);
    assert_eq!(json["outcome"], "stopped");
    // Not lost, just moved: it is still what git said, and it still belongs in
    // a log.
    assert!(
        err.contains("CONFLICT") || err.contains("could not apply"),
        "git's account of it has to survive somewhere: {err}"
    );
}

#[test]
fn a_stopped_rehearsal_hands_back_an_id_and_a_path_that_work() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);

    let (code, out, err) = fixture.rehearse(&["--json", "rebase", "main"]);
    assert_eq!(code, STOPPED, "{err}");
    let json = document(&out);

    // The whole point of #48, now in a form a program can act on.
    assert_eq!(json["decision"], "kept");
    assert_eq!(json["conflicted"], true);
    assert_eq!(json["can_apply"], false);
    assert_eq!(json["conflicts"][0]["path"], "file.txt");
    assert_eq!(json["stopped_at"]["subject"], "three");

    let id = json["id"].as_str().expect("an id").to_owned();
    let sandbox = std::path::PathBuf::from(json["sandbox"].as_str().expect("a sandbox path"));
    assert!(sandbox.is_dir(), "the path it gave has to exist");

    // Resolve it there and carry on, exactly as the document invites.
    std::fs::write(sandbox.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&sandbox, &["add", "file.txt"]);
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "continue", &id]);

    assert_eq!(code, CLEAN, "{err}\n{out}");
    let json = document(&out);
    assert_eq!(json["outcome"], "clean");
    assert_eq!(json["can_apply"], true);
    assert_eq!(json["decision"], "kept");
}

#[test]
fn a_resolution_that_changes_what_a_commit_does_says_so_in_a_field() {
    // The warning this tool exists for, machine-readable. An agent that cannot
    // see this has no reason to prefer rehearsing over just rebasing.
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (_, out, _) = fixture.rehearse(&["--json", "rebase", "main"]);
    let json = document(&out);
    let id = json["id"].as_str().expect("an id").to_owned();
    let sandbox = std::path::PathBuf::from(json["sandbox"].as_str().expect("a path"));

    // A value neither side ever had.
    std::fs::write(sandbox.join("file.txt"), "neither\n").expect("resolve");
    fixture.git_in(&sandbox, &["add", "file.txt"]);
    let (_, out, err) = fixture.rehearse(&["--json", "--keep", "continue", &id]);

    let json = document(&out);
    assert_eq!(json["drift_unexpected"], true, "{err}");
    assert_eq!(
        json["drift"][0]["replay"]["changed"][0], "three",
        "and it names the commit that changed: {json}"
    );
    assert_eq!(json["drift"][0]["replay"]["compared"], true);
}

#[test]
fn every_management_command_answers_in_json_too() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (_, out, _) = fixture.rehearse(&["--json", "rebase", "main"]);
    let id = document(&out)["id"].as_str().expect("an id").to_owned();

    let (code, out, _) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN);
    let json = document(&out);
    assert_eq!(json["schema"], 1);
    assert_eq!(json["rehearsals"][0]["id"], id.as_str());
    assert_eq!(json["rehearsals"][0]["status"], "kept");
    assert_eq!(json["rehearsals"][0]["outcome"], "stopped");
    assert_eq!(json["rehearsals"][0]["execution"], "stopped");
    let origin = fixture.repo().display().to_string();
    assert_eq!(json["rehearsals"][0]["origin_worktree"], origin);
    assert_eq!(json["rehearsals"][0]["repository"], origin);
    assert_eq!(json["rehearsals"][0]["lifecycle"], "kept");
    assert!(
        json["rehearsals"][0]["storage"]["metadata"]
            .as_str()
            .is_some()
    );

    let (code, out, _) = fixture.rehearse(&["--json", "show", &id]);
    assert_eq!(code, CLEAN);
    assert_eq!(document(&out)["outcome"], "stopped");

    let (code, out, _) = fixture.rehearse(&["--json", "discard", &id]);
    assert_eq!(code, CLEAN);
    assert_eq!(document(&out)["discarded"][0], id.as_str());
}

#[test]
fn an_unfinished_rehearsal_is_reported_as_incomplete_after_restart() {
    let fixture = Fixture::new();
    let plan = fixture.plan(
        &["merge", "feature"],
        git_rehearse::sandbox::Checkout::Branch("main".to_owned()),
    );
    let sandbox = git_rehearse::sandbox::create(fixture.cache(), &plan, git_rehearse::now_unix())
        .expect("sandbox exists before the command starts");
    let id = sandbox.id().to_owned();

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    let listed = document(&out);
    assert_eq!(listed["rehearsals"][0]["id"], id);
    assert_eq!(listed["rehearsals"][0]["execution"], "incomplete");
    assert!(listed["rehearsals"][0]["outcome"].is_null());

    let (code, out, err) = fixture.rehearse(&["--json", "show", &id]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    let shown = document(&out);
    assert_eq!(shown["schema"], 1);
    assert_eq!(shown["execution"], "incomplete");
    assert_eq!(shown["id"], id);

    let (code, out, err) = fixture.rehearse(&["show", &id]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    for detail in [
        "checkout Branch(\"main\")",
        "lifecycle Fresh",
        "pre-state refs 3",
        "execution incomplete",
    ] {
        assert!(out.contains(detail), "missing {detail}: {out}");
    }
}

#[test]
fn continuing_a_retained_rehearsal_keeps_it_without_a_new_decision() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);

    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "rebase", "main"]);
    assert_eq!(code, STOPPED, "{err}");
    let first = document(&out);
    let id = first["id"].as_str().expect("an id").to_owned();
    let sandbox = std::path::PathBuf::from(first["sandbox"].as_str().expect("a sandbox"));
    std::fs::write(sandbox.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&sandbox, &["add", "file.txt"]);

    let (code, out, err) = fixture.rehearse(&["--json", "continue", &id]);
    assert_eq!(code, CLEAN, "{err}");
    assert_eq!(document(&out)["decision"], "kept");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN, "{err}");
    assert_eq!(document(&out)["rehearsals"][0]["id"], id);
}

#[cfg(unix)]
#[test]
fn an_explicitly_kept_initial_run_survives_interruption_and_age_pruning() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let fixture = Fixture::new();
    let marker = fixture.base().join("editor-started");
    let editor = fixture.base().join("blocking-editor.sh");
    let stderr = fixture.base().join("rehearsal-stderr");
    // Git supplies the message path as an argument. A script ignores that
    // argument instead of accidentally passing it on to sleep.
    std::fs::write(
        &editor,
        "#!/bin/sh\ntouch \"$EDITOR_MARKER\"\nexec sleep 60\n",
    )
    .expect("blocking editor script");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("editor executable");
    let mut child = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .args(["--json", "--keep", "merge", "--no-ff", "--edit", "feature"])
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_EDITOR", &editor)
        .env("EDITOR_MARKER", &marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("rehearsal stderr"))
        .process_group(0)
        .spawn()
        .expect("rehearsal starts");
    for _ in 0..3000 {
        if marker.exists() || child.try_wait().expect("check rehearsal status").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let was_running = child.try_wait().expect("check rehearsal status").is_none();
    // Kill the CLI first so Git's death cannot be recorded as a completed
    // outcome. Then stop Git and its editor before any assertion can panic.
    if was_running {
        child.kill().expect("interrupt rehearsal");
    }
    let killed = Command::new("kill")
        // procps kill can parse -1234 as -1 without the option terminator,
        // signalling unrelated processes instead of this process group.
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .status()
        .expect("kill rehearsal process group");
    child.wait().expect("reap rehearsal");
    assert!(
        marker.exists(),
        "the merge reached its editor: {}",
        std::fs::read_to_string(&stderr).expect("rehearsal stderr")
    );
    assert!(
        was_running,
        "the rehearsal was still running before interruption"
    );
    assert!(killed.success(), "the rehearsal process group was killed");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    let listing = document(&out);
    let entry = &listing["rehearsals"][0];
    let id = entry["id"].as_str().expect("rehearsal id");
    let metadata = entry["storage"]["metadata"]
        .as_str()
        .expect("metadata path");
    // Move only the clock input forward relative to this record. Retention
    // and execution state remain exactly as the interrupted CLI wrote them.
    let mut saved = document(&std::fs::read_to_string(metadata).expect("metadata"));
    saved["created_unix"] = serde_json::json!(1);
    std::fs::write(metadata, serde_json::to_vec(&saved).expect("metadata JSON"))
        .expect("age rehearsal");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    let listing = document(&out);
    assert_eq!(listing["rehearsals"][0]["id"], id);
    assert_eq!(listing["rehearsals"][0]["lifecycle"], "kept");
    assert_eq!(listing["rehearsals"][0]["execution"], "incomplete");
    let (code, out, err) = fixture.rehearse(&["--json", "show", id]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    assert_eq!(document(&out)["execution"], "incomplete");
}

#[cfg(unix)]
#[test]
fn a_killed_continuation_is_reported_incomplete_after_restart() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "rebase", "main"]);
    assert_eq!(code, STOPPED, "{err}");
    let first = document(&out);
    let id = first["id"].as_str().expect("an id").to_owned();
    let sandbox = std::path::PathBuf::from(first["sandbox"].as_str().expect("a sandbox"));
    std::fs::write(sandbox.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(&sandbox, &["add", "file.txt"]);

    let editor = fixture.base().join("blocking-editor.sh");
    let marker = fixture.base().join("editor-started");
    let editor_pid = fixture.base().join("editor-pid");
    std::fs::write(
        &editor,
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$EDITOR_PID\"\ntouch \"$EDITOR_MARKER\"\nwhile :; do sleep 1; done\n",
    )
    .expect("editor script");
    let mut permissions = std::fs::metadata(&editor)
        .expect("editor metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&editor, permissions).expect("editor executable");

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .args(["--json", "--keep", "continue", &id])
        .current_dir(fixture.repo())
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_EDITOR", &editor)
        .env("EDITOR_MARKER", &marker)
        .env("EDITOR_PID", &editor_pid)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("continuation starts");
    for _ in 0..500 {
        if marker.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "the continuation reached the blocking editor"
    );

    child.kill().expect("kill continuation");
    let pid = std::fs::read_to_string(&editor_pid).expect("editor pid");
    let status = Command::new("kill")
        .args(["-KILL", pid.trim()])
        .status()
        .expect("kill process group");
    assert!(status.success(), "kill process group succeeded");
    let _ = child.wait().expect("continuation exits after kill");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    assert_eq!(document(&out)["rehearsals"][0]["execution"], "incomplete");
}

#[test]
fn text_and_json_management_report_the_same_repository_identity() {
    let fixture = Fixture::new();
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "merge", "--no-edit", "feature"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    let report = document(&out);
    let id = report["id"].as_str().expect("rehearsal id");
    let repository_id = report["repository_id"]
        .as_str()
        .expect("repository identity");

    for args in [vec!["list"], vec!["--stat-only", "show", id]] {
        let (code, out, err) = fixture.rehearse(&args);
        assert_eq!(code, CLEAN, "{out}\n{err}");
        assert!(
            out.contains(&format!("repository-id {repository_id} ")),
            "text output must identify the same repository as JSON: {out}"
        );
    }
}

#[test]
fn unavailable_origin_repository_has_no_substituted_repository_identity() {
    let fixture = Fixture::new();
    let plan = fixture.plan(
        &["merge", "feature"],
        git_rehearse::sandbox::Checkout::Branch("main".to_owned()),
    );
    let sandbox = git_rehearse::sandbox::create(fixture.cache(), &plan, git_rehearse::now_unix())
        .expect("sandbox");
    let metadata = sandbox.root().join("meta.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&metadata).expect("metadata"))
            .expect("metadata json");
    value["repo_path"] = serde_json::json!(fixture.base().join("repository-is-gone"));
    std::fs::write(
        &metadata,
        serde_json::to_vec(&value).expect("metadata json"),
    )
    .expect("write metadata");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    assert!(document(&out)["rehearsals"][0]["repository_id"].is_null());

    for args in [vec!["list"], vec!["show", sandbox.id()]] {
        let (code, out, err) = fixture.rehearse(&args);
        assert_eq!(code, CLEAN, "{out}\n{err}");
        assert!(out.contains("repository-id unavailable"), "{out}");
    }
}

#[test]
fn optional_metadata_survives_migration_and_later_continuation() {
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "rebase", "main"]);
    assert_eq!(code, STOPPED, "{out}\n{err}");
    let report = document(&out);
    let id = report["id"].as_str().expect("rehearsal id");
    let metadata = report["storage"]["metadata"]
        .as_str()
        .expect("metadata path");
    let worktree = std::path::Path::new(report["sandbox"].as_str().expect("sandbox"));
    let extension = serde_json::json!({"label": "keep my resolution", "notes": [1, null, true]});
    let mut saved = document(&std::fs::read_to_string(metadata).expect("metadata"));
    saved["schema"] = serde_json::json!(1);
    saved.as_object_mut().expect("object").remove("carry");
    saved["optional_annotation"] = extension.clone();
    std::fs::write(
        metadata,
        serde_json::to_vec(&saved).expect("legacy metadata"),
    )
    .expect("save legacy record");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    assert_eq!(document(&out)["rehearsals"][0]["id"], id);
    let migrated = document(&std::fs::read_to_string(metadata).expect("migrated metadata"));
    assert_eq!(migrated["schema"], git_rehearse::sandbox::META_SCHEMA);
    assert_eq!(migrated["optional_annotation"], extension);

    std::fs::write(worktree.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(worktree, &["add", "file.txt"]);
    let (code, out, err) = fixture.rehearse(&["--json", "continue", id]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    let continued = document(&std::fs::read_to_string(metadata).expect("continued metadata"));
    assert_eq!(continued["optional_annotation"], extension);
}

#[test]
fn unknown_metadata_is_a_json_refusal_without_deletion() {
    let fixture = Fixture::new();
    let plan = fixture.plan(
        &["merge", "feature"],
        git_rehearse::sandbox::Checkout::Branch("main".to_owned()),
    );
    let sandbox = git_rehearse::sandbox::create(fixture.cache(), &plan, git_rehearse::now_unix())
        .expect("sandbox");
    let metadata = sandbox.root().join("meta.json");
    std::fs::write(&metadata, r#"{"schema":999,"id":"preserve-me"}"#).expect("unknown metadata");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, REFUSED, "{out}\n{err}");
    let failure = document(&out);
    assert_eq!(failure["kind"], "refused");
    assert!(
        failure["message"]
            .as_str()
            .expect("message")
            .contains("schema 999")
    );
    assert!(
        metadata.exists(),
        "the unknown record remains available for diagnosis"
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_metadata_entry_is_refused_without_pruning_the_rehearsal() {
    use std::fs::{self, File, FileTimes};
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};

    let fixture = Fixture::new();
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "merge", "--no-edit", "feature"]);
    assert_eq!(code, CLEAN, "{out}\n{err}");
    let id = document(&out)["id"]
        .as_str()
        .expect("rehearsal id")
        .to_owned();
    let worktree = std::path::PathBuf::from(document(&out)["sandbox"].as_str().expect("sandbox"));
    let sandbox = worktree.parent().expect("sandbox root").to_owned();
    let metadata = sandbox.join("meta.json");

    fs::remove_file(&metadata).expect("remove metadata file");
    symlink("meta.json", &metadata).expect("self-referential metadata symlink");
    let old = SystemTime::now() - Duration::from_secs(DEFAULT_TTL_SECS + 86_400);
    File::open(&sandbox)
        .expect("open rehearsal directory")
        .set_times(FileTimes::new().set_modified(old))
        .expect("age rehearsal directory");

    let (code, out, err) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, REFUSED, "{out}\n{err}");
    assert_eq!(document(&out)["kind"], "refused");
    assert!(
        document(&out)["message"]
            .as_str()
            .expect("refusal message")
            .contains("meta.json")
    );
    assert!(
        sandbox.is_dir(),
        "the damaged rehearsal remains for recovery"
    );
    assert_eq!(
        id,
        sandbox.file_name().expect("sandbox id").to_string_lossy()
    );
}

#[test]
fn applying_reports_what_moved_and_where_the_undo_is() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");

    let (code, out, err) =
        fixture.rehearse(&["--json", "--apply", "merge", "--no-edit", "feature"]);

    assert_eq!(code, CLEAN, "{err}");
    let json = document(&out);
    assert_eq!(json["decision"], "applied");
    let applied = &json["applied"];
    assert!(
        applied["refs"]
            .as_array()
            .expect("refs")
            .iter()
            .any(|moved| moved["name"] == "refs/heads/main"),
        "{json}"
    );
    assert!(
        applied["undo"]
            .as_str()
            .expect("undo path")
            .ends_with("rehearse-undo"),
        "{json}"
    );
}

#[test]
fn undoing_is_a_document_too_and_so_is_having_nothing_to_undo() {
    let fixture = Fixture::new();
    fixture.commit_file("other.txt", "other\n", "four");
    let before = fixture.git(&["rev-parse", "main"]);
    let (_, out, _) = fixture.rehearse(&["--json", "--apply", "merge", "--no-edit", "feature"]);
    let id = document(&out)["id"].as_str().expect("an id").to_owned();

    let (code, out, err) = fixture.rehearse(&["--json", "undo"]);

    assert_eq!(code, CLEAN, "{err}");
    let json = document(&out);
    assert_eq!(json["schema"], 1);
    assert_eq!(json["exit_code"], 0);
    // Which apply was taken back, since there is one record and no prompt: a
    // caller that applied twice can tell whether this was the one it meant.
    assert_eq!(json["rehearsal"], id.as_str());
    assert!(json["applied_at_unix"].is_number(), "{json}");
    assert!(
        json["restored"]
            .as_array()
            .expect("restored")
            .iter()
            .any(|restored| restored["name"] == "refs/heads/main"),
        "{json}"
    );
    assert_eq!(fixture.git(&["rev-parse", "main"]), before);

    let (code, out, _) = fixture.rehearse(&["--json", "undo"]);
    assert_eq!(code, REFUSED);
    let json = document(&out);
    assert_eq!(json["kind"], "refused");
    assert!(
        json["message"]
            .as_str()
            .expect("a message")
            .contains("nothing to undo"),
        "{json}"
    );
}

#[test]
fn a_failure_is_a_document_as_well() {
    // A caller that parses JSON on success and meets English on failure has to
    // parse English anyway, so every exit path emits one.
    let fixture = Fixture::new();
    fixture.commit_file(
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "track binaries with lfs",
    );

    let (code, out, err) = fixture.rehearse(&["--json", "merge", "feature"]);

    assert_eq!(code, REFUSED);
    let json = document(&out);
    assert_eq!(json["kind"], "refused");
    assert_eq!(json["exit_code"], 4);
    assert!(
        json["message"]
            .as_str()
            .expect("a message")
            .contains("Git LFS"),
        "{json}"
    );
    // And the human still gets it on stderr, which nobody parses.
    assert!(err.contains("Git LFS"), "{err}");
}

#[test]
fn even_a_refusal_from_the_parser_itself_is_a_document() {
    // The run never reached a command, so there is no parse to read the format
    // out of — main() has to find --json on the argument list.
    let fixture = Fixture::new();

    let (code, out, _) = fixture.rehearse(&["--json", "status"]);

    assert_eq!(code, REFUSED);
    let json = document(&out);
    assert_eq!(json["kind"], "refused");
    assert!(
        json["message"]
            .as_str()
            .expect("a message")
            .contains("git rehearse -- status"),
        "{json}"
    );
}

#[test]
fn a_refused_apply_tells_the_caller_where_the_rehearsal_went() {
    // A program cannot go looking in a cache directory. If the refusal keeps
    // the sandbox, the document is the only place that fact can reach it.
    let fixture = Fixture::new();
    fixture.commit("four", "four\n");
    fixture.git(&["checkout", "feature"]);

    let (code, out, err) = fixture.rehearse(&["--json", "--apply", "rebase", "main"]);

    assert_eq!(code, REFUSED, "{err}");
    let json = document(&out);
    assert_eq!(json["kind"], "refused");
    let message = json["message"].as_str().expect("a message");
    assert!(message.contains("kept as"), "{message}");
    assert!(message.contains("git rehearse continue"), "{message}");

    // And it really is there, under the id the message names.
    let (code, out, _) = fixture.rehearse(&["--json", "list"]);
    assert_eq!(code, CLEAN);
    let listed = document(&out);
    let id = listed["rehearsals"][0]["id"].as_str().expect("an id");
    assert_eq!(listed["rehearsals"][0]["status"], "kept");
    assert!(message.contains(id), "{message}");
}

#[test]
fn a_command_git_refuses_carries_gits_own_exit_code() {
    let fixture = Fixture::new();

    let (code, out, err) = fixture.rehearse(&["--json", "merge", "no-such-branch"]);

    assert_eq!(code, FAILED, "{err}");
    let json = document(&out);
    assert_eq!(json["outcome"], "failed");
    assert_eq!(json["exit_code"], 3);
    assert!(json["git_exit_code"].is_number(), "{json}");
    // Nothing to come back for, so it is not kept.
    assert_eq!(json["decision"], "discarded");
}
