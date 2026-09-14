//! Hook policy through the public CLI, with real executable sentinel hooks.
mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use support::Fixture;

fn hooks(fixture: &Fixture) -> std::path::PathBuf {
    let directory = fixture.scratch("sentinel-hooks");
    for event in [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "pre-rebase",
        "post-rewrite",
        "post-checkout",
        "post-merge",
        "reference-transaction",
        "pre-push",
        "pre-receive",
        "update",
        "post-receive",
    ] {
        let path = directory.join(event);
        std::fs::write(&path, "#!/bin/sh\nprintf 'ran\\n' >> \"$HOOK_SENTINEL\"\n").expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("executable");
        }
    }
    directory
}

fn run(fixture: &Fixture, hooks: &Path, source: &str, args: &[&str]) -> (i32, serde_json::Value) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_git-rehearse"));
    command
        .current_dir(fixture.repo())
        .args(["--json", "--keep"])
        .args(args)
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", fixture.base().join("global-config"))
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_EDITOR", "true")
        .env("HOOK_SENTINEL", fixture.base().join("sentinel"))
        .stdin(Stdio::null());
    let path = hooks.to_str().expect("path");
    match source {
        "local" => {
            fixture.git(&["config", "core.hooksPath", path]);
        }
        "global" => {
            fixture.git(&[
                "config",
                "--file",
                fixture.base().join("global-config").to_str().expect("path"),
                "core.hooksPath",
                path,
            ]);
        }
        "count" => {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.hooksPath")
                .env("GIT_CONFIG_VALUE_0", path);
        }
        "parameters" => {
            command.env(
                "GIT_CONFIG_PARAMETERS",
                format!("'core.hooksPath={}'", path.replace('\'', "'\\''")),
            );
        }
        _ => panic!("unknown source"),
    }
    let output = command.output().expect("CLI");
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {output:?}"));
    (output.status.code().expect("exit"), json)
}

#[test]
fn hooks_cannot_be_enabled_during_creation_execution_or_apply() {
    for source in ["local", "global", "count", "parameters"] {
        let fixture = Fixture::new();
        let hooks = hooks(&fixture);
        // Carry transfer invokes push/receive-pack as well as clone/checkout.
        std::fs::write(fixture.repo().join("file.txt"), "dirty\n").expect("dirty work");
        let (code, report) = run(
            &fixture,
            &hooks,
            source,
            &[
                "--",
                "-c",
                &format!("core.hooksPath={}", hooks.display()),
                "commit",
                "--allow-empty",
                "-m",
                "rehearsed",
            ],
        );
        assert_eq!(code, 0, "{source}: {report}");
        assert_eq!(report["repository_hooks"], "disabled");
        let id = report["id"].as_str().expect("id");
        let (code, applied) = run(&fixture, &hooks, source, &["apply", id]);
        assert_eq!(code, 0, "{source}: {applied}");
        assert_eq!(applied["repository_hooks"], "disabled");
        assert!(
            !fixture.base().join("sentinel").exists(),
            "{source}: hook ran"
        );
    }
}

#[test]
fn continue_ignores_changed_sandbox_hook_configuration() {
    let fixture = Fixture::new();
    fixture.commit("diverge", "main\n");
    fixture.git(&["checkout", "feature"]);
    let hooks = hooks(&fixture);
    let (code, report) = run(&fixture, &hooks, "count", &["rebase", "main"]);
    assert_eq!(code, 2, "{report}");
    let sandbox = Path::new(report["sandbox"].as_str().expect("sandbox"));
    fixture.git_in(
        sandbox,
        &["config", "core.hooksPath", hooks.to_str().expect("path")],
    );
    std::fs::write(sandbox.join("file.txt"), "resolved\n").expect("resolve");
    fixture.git_in(sandbox, &["add", "file.txt"]);
    let id = report["id"].as_str().expect("id");
    let (code, report) = run(&fixture, &hooks, "parameters", &["continue", id]);
    assert_eq!(code, 0, "{report}");
    let (code, report) = run(&fixture, &hooks, "parameters", &["apply", id]);
    assert_eq!(code, 0, "{report}");
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("file"),
        "resolved\n"
    );
    assert!(!fixture.base().join("sentinel").exists());
}

#[test]
fn refused_apply_preserves_refs_index_and_worktree_without_hooks() {
    let fixture = Fixture::new();
    let hooks = hooks(&fixture);
    let (code, report) = run(&fixture, &hooks, "local", &["merge", "feature"]);
    assert_eq!(code, 0, "{report}");
    std::fs::write(fixture.repo().join("file.txt"), "new local work\n").expect("edit");
    let refs = fixture.refs();
    let index = std::fs::read(fixture.repo().join(".git/index")).expect("index");
    let (code, report) = run(
        &fixture,
        &hooks,
        "local",
        &["apply", report["id"].as_str().expect("id")],
    );
    assert_eq!(code, 4, "{report}");
    assert_eq!(fixture.refs(), refs);
    assert_eq!(
        std::fs::read(fixture.repo().join(".git/index")).expect("index"),
        index
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo().join("file.txt")).expect("file"),
        "new local work\n"
    );
    assert!(!fixture.base().join("sentinel").exists());
}

#[test]
fn direct_git_still_runs_the_sentinel() {
    let fixture = Fixture::new();
    let hooks = hooks(&fixture);
    let output = Command::new("git")
        .current_dir(fixture.repo())
        .args([
            "-c",
            &format!("core.hooksPath={}", hooks.display()),
            "commit",
            "--allow-empty",
            "-m",
            "direct",
        ])
        .env("HOOK_SENTINEL", fixture.base().join("sentinel"))
        .output()
        .expect("direct Git");
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.base().join("sentinel").exists());
}

#[test]
fn text_report_explains_the_hook_policy() {
    let fixture = Fixture::new();
    let (code, report, error) = fixture.rehearse(&["--keep", "merge", "feature"]);
    assert_eq!(code, 0, "{error}");
    assert!(report.contains("Repository hooks were not run"), "{report}");
}

#[test]
fn git_aliases_cannot_inject_a_later_hook_override() {
    for inherited in [false, true] {
        let fixture = Fixture::new();
        let hooks = hooks(&fixture);
        let alias = format!(
            "-c core.hooksPath='{}' commit --allow-empty -m aliased",
            hooks.display()
        );
        let setting = format!("alias.save={alias}");
        if inherited {
            fixture.git(&[
                "config",
                "--file",
                fixture.base().join("global-config").to_str().expect("path"),
                "alias.save",
                &alias,
            ]);
        }
        let refs = fixture.refs();
        let index = std::fs::read(fixture.repo().join(".git/index")).expect("index");
        let content = std::fs::read(fixture.repo().join("file.txt")).expect("file");
        let args = if inherited {
            vec!["--", "save"]
        } else {
            vec!["--", "-c", &setting, "save"]
        };
        let (code, report) = run(&fixture, &hooks, "global", &args);
        assert_eq!(code, 4, "{report}");
        assert!(report.to_string().contains("alias"), "{report}");
        assert_eq!(fixture.refs(), refs);
        assert_eq!(
            std::fs::read(fixture.repo().join(".git/index")).expect("index"),
            index
        );
        assert_eq!(
            std::fs::read(fixture.repo().join("file.txt")).expect("file"),
            content
        );
        assert!(!fixture.base().join("sentinel").exists());
    }
}
