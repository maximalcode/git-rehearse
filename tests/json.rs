//! `--json`, driven the way a program would drive it.
//!
//! These tests parse the process's real stdout rather than inspecting a
//! serialised struct, because the contract being tested is *"one document on
//! stdout and nothing else"* — and the way that broke in development was git
//! writing `Auto-merging …` to the same stream, which no amount of testing the
//! `Serialize` impl would have caught.

mod support;

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

    let (code, out, _) = fixture.rehearse(&["--json", "show", &id]);
    assert_eq!(code, CLEAN);
    assert_eq!(document(&out)["outcome"], "stopped");

    let (code, out, _) = fixture.rehearse(&["--json", "discard", &id]);
    assert_eq!(code, CLEAN);
    assert_eq!(document(&out)["discarded"][0], id.as_str());
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
fn a_failure_is_a_document_as_well() {
    // A caller that parses JSON on success and meets English on failure has to
    // parse English anyway, so every exit path emits one.
    let fixture = Fixture::new();
    fixture.write("file.txt", "uncommitted\n");

    let (code, out, err) = fixture.rehearse(&["--json", "merge", "feature"]);

    assert_eq!(code, REFUSED);
    let json = document(&out);
    assert_eq!(json["kind"], "refused");
    assert_eq!(json["exit_code"], 4);
    assert!(
        json["message"]
            .as_str()
            .expect("a message")
            .contains("uncommitted changes"),
        "{json}"
    );
    // And the human still gets it on stderr, which nobody parses.
    assert!(err.contains("uncommitted changes"), "{err}");
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
