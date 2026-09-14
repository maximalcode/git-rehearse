//! Signing guarantees through the public CLI and real temporary SSH keys.
mod support;

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use support::Fixture;

fn run(fixture: &Fixture, args: &[&str]) -> (Output, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(fixture.repo())
        .args(["--json", "--keep"])
        .args(args)
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", fixture.base().join("global-config"))
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("SSH_AUTH_SOCK", fixture.base().join("agent.sock"))
        // JSON operations must not launch the configured terminal editor.
        .env("GIT_EDITOR", "an-editor-that-does-not-exist")
        .stdin(Stdio::null())
        .output()
        .expect("CLI runs");
    let json = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {output:?}"));
    (output, json)
}

fn clean(fixture: &Fixture, args: &[&str]) -> Value {
    let (output, json) = run(fixture, args);
    assert!(output.status.success(), "{output:?}: {json}");
    json
}

fn verify_and_apply(fixture: &Fixture, report: &Value) {
    let sandbox = Path::new(report["sandbox"].as_str().expect("sandbox"));
    let sha = fixture.git_in(sandbox, &["rev-parse", "HEAD"]);
    let object = fixture.git_in(sandbox, &["cat-file", "commit", &sha]);
    assert!(
        object.contains("\ngpgsig -----BEGIN SSH SIGNATURE-----"),
        "{object}"
    );
    let entries = report["signatures"].as_array().expect("signatures");
    let signature = entries
        .iter()
        .find(|entry| entry["sha"] == sha)
        .expect("tip signature");
    assert_eq!(signature["present"], true);
    assert_eq!(signature["verification"], "not_checked");
    assert_eq!(signature["trust"], "not_checked");
    let allowed = fixture.base().join("allowed-signers");
    let public =
        std::fs::read_to_string(fixture.base().join("signing-key.pub")).expect("public key");
    std::fs::write(&allowed, format!("fixture@example.invalid {public}")).expect("allowed signers");
    fixture.git_in(
        sandbox,
        &[
            "-c",
            &format!("gpg.ssh.allowedSignersFile={}", allowed.display()),
            "verify-commit",
            &sha,
        ],
    );
    let shown = clean(fixture, &["show", report["id"].as_str().expect("id")]);
    assert_eq!(shown["signatures"], report["signatures"]);
    // Applying must need no signer and must preserve the exact object.
    fixture.git(&["config", "gpg.ssh.program", "missing-signer-at-apply"]);
    clean(fixture, &["apply", report["id"].as_str().expect("id")]);
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), sha);
    assert_eq!(fixture.git(&["cat-file", "commit", "HEAD"]), object);
}

#[test]
fn merge_rebase_and_cherry_pick_produce_signed_objects_and_apply_them_unchanged() {
    for operation in ["merge", "rebase", "cherry-pick"] {
        let fixture = Fixture::new();
        fixture.commit_file("other.txt", "independent\n", "independent");
        fixture.sign_with_ssh();
        let report = clean(&fixture, &[operation, "feature"]);
        verify_and_apply(&fixture, &report);
    }
}

#[test]
fn continue_signs_resolved_merge_rebase_and_cherry_pick_results() {
    for operation in ["merge", "rebase", "cherry-pick"] {
        let fixture = Fixture::new();
        fixture.commit("diverge", "conflicting main\n");
        fixture.sign_with_ssh();
        let (output, stopped) = run(&fixture, &[operation, "feature"]);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let sandbox = Path::new(stopped["sandbox"].as_str().expect("sandbox"));
        std::fs::write(sandbox.join("file.txt"), "resolved\n").expect("resolution");
        fixture.git_in(sandbox, &["add", "file.txt"]);
        let report = clean(&fixture, &["continue", stopped["id"].as_str().expect("id")]);
        verify_and_apply(&fixture, &report);
    }
}

#[test]
fn missing_keys_and_missing_or_unusable_signers_never_fall_back_to_unsigned() {
    for failure in ["key", "missing-program", "unusable-program"] {
        for operation in ["merge", "rebase", "cherry-pick"] {
            let fixture = Fixture::new();
            fixture.commit_file("other.txt", "independent\n", "independent");
            fixture.sign_with_ssh();
            match failure {
                "key" => {
                    fixture.git(&["config", "user.signingkey", "missing-key"]);
                }
                "missing-program" => {
                    fixture.git(&["config", "gpg.ssh.program", "missing-signing-program"]);
                }
                _ => {
                    fixture.git(&["config", "gpg.ssh.program", "git"]);
                }
            }
            let before = fixture.git(&["rev-parse", "HEAD"]);
            let (output, report) = run(&fixture, &[operation, "feature"]);
            assert!(!output.status.success(), "{failure} {operation}: {report}");
            assert!(
                !output.stderr.is_empty(),
                "Git's signing error must be visible"
            );
            assert_eq!(report["can_apply"], false, "{report}");
            assert_eq!(fixture.git(&["rev-parse", "HEAD"]), before);
            let (apply, _) = run(&fixture, &["apply", report["id"].as_str().expect("id")]);
            assert_eq!(apply.status.code(), Some(4));
        }
    }
}

#[test]
fn unsigned_results_are_reported_without_confusing_commit_messages_with_signatures() {
    let fixture = Fixture::new();
    let report = clean(
        &fixture,
        &[
            "--",
            "commit",
            "--allow-empty",
            "-m",
            "gpgsig fake signature",
        ],
    );
    let entries = report["signatures"].as_array().expect("signatures");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["present"], false);
    assert_eq!(entries[0]["verification"], "not_checked");
}

#[test]
fn relative_ssh_keys_are_resolved_from_the_origin() {
    let fixture = Fixture::new();
    fixture.sign_with_ssh();
    fixture.git(&["config", "user.signingkey", "../signing-key.pub"]);
    let report = clean(&fixture, &["merge", "--no-ff", "feature"]);
    verify_and_apply(&fixture, &report);
}

#[test]
fn an_empty_local_signing_setting_overrides_a_global_true_setting() {
    let fixture = Fixture::new();
    fixture.git(&[
        "config",
        "--file",
        fixture.base().join("global-config").to_str().expect("path"),
        "commit.gpgsign",
        "true",
    ]);
    fixture.git(&["config", "commit.gpgsign", ""]);
    let report = clean(&fixture, &["merge", "--no-ff", "feature"]);
    assert!(
        report["signatures"]
            .as_array()
            .expect("signatures")
            .iter()
            .all(|entry| entry["present"] == false)
    );
}

// Unix can start an isolated agent without relying on a system service.
#[cfg(unix)]
#[test]
fn ssh_default_key_command_is_carried_and_executed() {
    struct Agent(std::process::Child);
    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fixture = Fixture::new();
    let public = fixture.sign_with_ssh();
    let socket = fixture.base().join("agent.sock");
    let _agent = Agent(
        Command::new("ssh-agent")
            .args(["-D", "-a"])
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("isolated SSH agent"),
    );
    let mut loaded = false;
    for _ in 0..40 {
        let status = Command::new("ssh-add")
            .arg(fixture.base().join("signing-key"))
            .env("SSH_AUTH_SOCK", &socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("ssh-add");
        if status.success() {
            loaded = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(loaded, "test key must load into the isolated agent");
    fixture.git(&["config", "--unset", "user.signingkey"]);
    let key = std::fs::read_to_string(public).expect("public key");
    let command = format!("printf '%s\\n' 'key::{}'", key.trim());
    fixture.git(&["config", "gpg.ssh.defaultKeyCommand", &command]);
    let report = clean(&fixture, &["merge", "--no-ff", "feature"]);
    verify_and_apply(&fixture, &report);
}

#[test]
fn signing_failure_during_continue_leaves_no_unsigned_result() {
    for operation in ["merge", "rebase", "cherry-pick"] {
        let fixture = Fixture::new();
        fixture.commit("diverge", "conflicting main\n");
        fixture.sign_with_ssh();
        let (_, stopped) = run(&fixture, &[operation, "feature"]);
        let sandbox = Path::new(stopped["sandbox"].as_str().expect("sandbox"));
        let before = fixture.git_in(sandbox, &["rev-parse", "HEAD"]);
        std::fs::write(sandbox.join("file.txt"), "resolved\n").expect("resolution");
        fixture.git_in(sandbox, &["add", "file.txt"]);
        fixture.git_in(
            sandbox,
            &["config", "gpg.ssh.program", "missing-signing-program"],
        );
        let (output, report) = run(&fixture, &["continue", stopped["id"].as_str().expect("id")]);
        assert!(!output.status.success(), "{report}");
        assert!(!output.stderr.is_empty());
        assert_eq!(report["can_apply"], false);
        assert_eq!(fixture.git_in(sandbox, &["rev-parse", "HEAD"]), before);
    }
}

#[test]
fn every_intermediate_rebased_commit_has_its_own_signature_entry() {
    let fixture = Fixture::new();
    fixture.commit_file("a.txt", "a\n", "first replay");
    fixture.commit_file("b.txt", "b\n", "second replay");
    fixture.sign_with_ssh();
    let report = clean(&fixture, &["rebase", "feature"]);
    let sandbox = Path::new(report["sandbox"].as_str().expect("sandbox"));
    let rewritten = fixture.git_in(sandbox, &["rev-list", "feature..HEAD"]);
    assert_eq!(rewritten.lines().count(), 2);
    for sha in rewritten.lines() {
        let signature = report["signatures"]
            .as_array()
            .expect("signatures")
            .iter()
            .find(|entry| entry["sha"] == sha)
            .expect("intermediate signature");
        assert_eq!(signature["present"], true);
        assert!(fixture.is_signed(sandbox, sha));
    }
}

#[test]
fn a_valueless_signing_boolean_still_means_true() {
    use std::io::Write as _;
    let fixture = Fixture::new();
    fixture.sign_with_ssh();
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.repo().join(".git/config"))
        .expect("config");
    writeln!(config, "\n[commit]\n    gpgsign").expect("implicit true");
    let report = clean(&fixture, &["merge", "--no-ff", "feature"]);
    verify_and_apply(&fixture, &report);
}

#[test]
fn openpgp_program_aliases_keep_their_original_precedence() {
    for names in [
        ["gpg.program", "gpg.openpgp.program"],
        ["gpg.openpgp.program", "gpg.program"],
    ] {
        let fixture = Fixture::new();
        fixture.git(&["config", "commit.gpgsign", "true"]);
        fixture.git(&["config", names[0], "obsolete-signing-program"]);
        fixture.git(&["config", names[1], "effective-signing-program"]);
        let (output, report) = run(&fixture, &["merge", "--no-ff", "feature"]);
        assert!(!output.status.success());
        assert_eq!(report["can_apply"], false);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("effective-signing-program"),
            "{output:?}"
        );
    }
}
