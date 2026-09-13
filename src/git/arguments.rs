//! Locating Git's command after its global options, without changing arguments.
use std::ffi::OsString;

pub(super) fn command_boundary(args: &[OsString]) -> usize {
    let mut boundary = 0;
    while let Some(arg) = args.get(boundary) {
        let arg = arg.to_string_lossy();
        if !arg.starts_with('-')
            || matches!(arg.as_ref(), "--" | "--version" | "-v" | "--help" | "-h")
        {
            break;
        }
        boundary += 1;
        if matches!(
            arg.as_ref(),
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
        ) {
            boundary = (boundary + 1).min(args.len());
        }
    }
    boundary
}

#[cfg(test)]
mod tests {
    use super::command_boundary;
    use std::ffi::OsString;

    #[test]
    fn configuration_is_inserted_after_global_options_before_the_command() {
        for (args, expected) in [
            (vec!["merge", "--no-edit", "feature"], 0),
            (vec!["-c", "core.hooksPath=hooks", "commit"], 2),
            (vec!["-C", "a path", "-c", "a.b=c", "commit"], 4),
            (vec!["--config-env=core.hooksPath=HOOKS", "commit"], 1),
            (vec!["-cfoo.bar=baz", "--no-pager", "log"], 2),
            (
                vec!["--git-dir", "repo.git", "--work-tree=repo", "status"],
                3,
            ),
            (vec!["--version"], 0),
            (vec!["-h"], 0),
            (vec!["--"], 0),
            (vec!["-c"], 1),
            (vec![], 0),
        ] {
            let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
            assert_eq!(command_boundary(&args), expected, "{args:?}");
        }
    }
}
