//! Real Git evidence through the public JSON CLI.
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use support::Fixture;

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    if root.exists() {
        for entry in fs::read_dir(root).expect("cache readable") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                for (name, bytes) in snapshot(&path) {
                    result.insert(PathBuf::from(path.file_name().unwrap()).join(name), bytes);
                }
            } else {
                result.insert(
                    path.file_name().unwrap().into(),
                    fs::read(&path).expect("bytes"),
                );
            }
        }
    }
    result
}

fn rehearsal(fixture: &Fixture) -> serde_json::Value {
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "merge", "--no-edit", "feature"]);
    assert_eq!(code, 2, "{out}\n{err}");
    serde_json::from_str(&out).expect("JSON")
}

#[test]
fn rerere_learns_only_in_sandbox_and_reuses_an_independent_copy() {
    let fixture = Fixture::new();
    fixture.commit("diverge", "main side\n");
    fixture.git(&["config", "rerere.enabled", "true"]);
    fixture.git(&["config", "rerere.autoupdate", "true"]);
    let cache = fixture.repo().join(".git/rr-cache");
    let first = rehearsal(&fixture);
    let sandbox = Path::new(first["sandbox"].as_str().unwrap());
    assert_eq!(first["rerere_resolution_transfer"], "sandbox_only");
    fs::write(sandbox.join("file.txt"), "resolved\n").unwrap();
    fixture.git_in(sandbox, &["add", "file.txt"]);
    let id = first["id"].as_str().unwrap();
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "continue", id]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(!cache.exists(), "learning must not create the real cache");
    // Seed the original with a resolution learned by real Git, then rehearse
    // the same conflict again to prove Git consumes the copied resolution.
    let learned = snapshot(&sandbox.join(".git/rr-cache"));
    assert!(learned.keys().any(|path| path.ends_with("postimage")));
    for (path, bytes) in &learned {
        if path.components().count() > 1 {
            fs::create_dir_all(cache.join(path).parent().unwrap()).unwrap();
            fs::write(cache.join(path), bytes).unwrap();
        }
    }
    // Git also enables rerere implicitly when an rr-cache already exists.
    fixture.git(&["config", "--unset", "rerere.enabled"]);
    let before = snapshot(&cache);
    let second = rehearsal(&fixture);
    let second_path = Path::new(second["sandbox"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(second_path.join("file.txt")).unwrap(),
        "resolved\n"
    );
    assert!(fixture.git_in(second_path, &["ls-files", "-u"]).is_empty());
    let id = second["id"].as_str().unwrap();
    let (code, out, err) = fixture.rehearse(&["--json", "--keep", "continue", id]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(snapshot(&cache), before);
    let postimage = learned
        .keys()
        .find(|path| path.ends_with("postimage"))
        .unwrap();
    fs::write(
        second_path.join(".git/rr-cache").join(postimage),
        "sandbox edit\n",
    )
    .unwrap();
    assert_eq!(
        snapshot(&cache),
        before,
        "cache files must not be hardlinked"
    );
    let (code, out, err) = fixture.rehearse(&["--json", "apply", id]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(snapshot(&cache), before);
    assert_eq!(
        fs::read_to_string(fixture.repo().join("file.txt")).unwrap(),
        "resolved\n"
    );
}

#[cfg(unix)]
#[test]
fn rerere_cache_read_errors_are_not_treated_as_absence() {
    let fixture = Fixture::new();
    let cache = fixture.repo().join(".git/rr-cache");
    fs::create_dir(&cache).unwrap();
    std::os::unix::fs::symlink(fixture.base().join("missing"), cache.join("broken")).unwrap();
    let (code, out, _) = fixture.rehearse(&["--json", "--keep", "merge", "feature"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("rerere cache entry"), "{out}");
}

#[test]
fn linked_worktree_copies_shared_cache_and_effective_worktree_settings() {
    let fixture = Fixture::new();
    let linked = fixture.base().join("linked");
    fixture.git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
    fixture.git(&["config", "extensions.worktreeConfig", "true"]);
    fixture.git(&["config", "rerere.enabled", "true"]);
    fixture.git_in(
        &linked,
        &["config", "--worktree", "rerere.enabled", "false"],
    );
    fixture.git_in(
        &linked,
        &["config", "--worktree", "rerere.autoupdate", "true"],
    );
    let cache = fixture.repo().join(".git/rr-cache/example");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("preimage"), "cache bytes\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_git-rehearse"))
        .current_dir(&linked)
        .args(["--json", "--keep", "merge", "--no-edit", "feature"])
        .env("GIT_REHEARSE_CACHE_DIR", fixture.cache())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sandbox = Path::new(report["sandbox"].as_str().unwrap());
    assert_eq!(
        fixture.git_in(sandbox, &["config", "--bool", "rerere.enabled"]),
        "false"
    );
    assert_eq!(
        fixture.git_in(sandbox, &["config", "--bool", "rerere.autoupdate"]),
        "true"
    );
    assert_eq!(
        fs::read(sandbox.join(".git/rr-cache/example/preimage")).unwrap(),
        b"cache bytes\n"
    );
}
