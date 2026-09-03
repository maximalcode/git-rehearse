//! Putting the refs back where `apply` found them.
//!
//! Applying is a transaction with an inverse, and this is the inverse. It is
//! **not** a re-run of anything either — design principle 2 points both ways.
//! Undoing a transplant is another transplant, back along the same edge, made
//! out of commit ids that were written down before the first one happened.
//!
//! Nothing has to be fetched to do it. The commits an apply moved away from are
//! still in the repository: an apply removes no object, it only stops
//! referencing one, and git's reflog keeps an unreferenced commit reachable for
//! weeks. (The rehearsed commits are equally safe the other way — apply parks
//! them under `refs/rehearse/<id>/*`, so an undo cannot orphan them and a
//! re-apply is not a re-rehearsal.)
//!
//! # The record, and why it holds both sides
//!
//! The file this module reads is the file [`write`] wrote, before the apply
//! moved anything, so that a crash mid-apply still leaves the way back written
//! down. It records where every ref *was* and where the apply *put* it, and it
//! needs both halves for one reason: apply's transaction states every expected
//! old value, so git itself refuses the batch if a ref moved underneath
//! ([`crate::apply`]). Undo wants the identical guarantee, and the pre-state
//! alone cannot give it — with nothing to state as the expected old value, an
//! unconditional restore would silently clobber whatever was committed after
//! the apply. Re-deriving the post-apply values from the sandbox instead was
//! the alternative, and it is worse: the sandbox is prunable and usually gone,
//! and a feature that works only while a cache entry survives is worse than one
//! that refuses.
//!
//! # One level deep
//!
//! There is one record per repository, at a fixed path, so **a second apply
//! overwrites the first record**. That is deliberate: undo means "take back the
//! thing I just did", not "walk my history backwards" — git-branchless already
//! owns the latter and does it far better than a fixed filename ever could. The
//! record names its rehearsal and the moment it was applied, both of which are
//! printed, and `undo <id>` refuses outright if the record is not the apply the
//! caller had in mind.
//!
//! A successful undo **consumes** the record. Leaving it would make a second
//! `undo` look idempotent while being an unrelated and unsafe operation;
//! deleting it makes the second one a clean "there is nothing to undo".

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::analyze::RefMove;
use crate::preflight::HEAD_KEY;
use crate::{Error, Result, git};

/// Where the undo record is written, inside the real repository's git dir.
pub const UNDO_FILE: &str = "rehearse-undo";

/// Version of the record format below.
///
/// `meta.json` has had [`crate::sandbox::META_SCHEMA`] since the first commit
/// and this file had nothing, which meant every future change to it would have
/// been a guess about what wrote it. Bump on any change that a reader of an
/// older record could get wrong.
pub const RECORD_SCHEMA: u32 = 1;

/// Git's own spelling for "this ref does not exist", used in the record for a
/// missing side.
///
/// Not a private placeholder: it is what git prints and what git accepts, so a
/// line naming it stays a working `git update-ref` invocation — as the new
/// value it deletes a branch the apply created, as the old value it insists a
/// branch the apply deleted is still absent.
const ABSENT: &str = "0000000000000000000000000000000000000000";

const VERSION_KEY: &str = "version";
const REHEARSAL_KEY: &str = "rehearsal";
const APPLIED_AT_KEY: &str = "applied-at";

/// What one apply did, in the form that lets it be taken back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The rehearsal that was applied.
    pub rehearsal: String,
    /// When it was applied, seconds since the Unix epoch.
    pub applied_at_unix: u64,
    /// Every ref the apply moved and an undo can move back: `before` is where
    /// it was, `after` is where the apply put it. Never `HEAD` — see [`render`].
    pub refs: Vec<RefMove>,
}

impl Record {
    /// The record for an apply that is about to move `moved`.
    ///
    /// `HEAD` is dropped here rather than at the point of writing, so that the
    /// invariant belongs to the record itself: every entry names something the
    /// undo transaction can act on. Dropping it loses nothing — `HEAD` is
    /// symbolic, it moved only because its branch did, and that branch is in
    /// the list with the same two commits.
    #[must_use]
    pub fn of_apply(rehearsal: String, applied_at_unix: u64, moved: &[RefMove]) -> Self {
        Self {
            rehearsal,
            applied_at_unix,
            refs: moved
                .iter()
                .filter(|moved| moved.name != HEAD_KEY)
                .cloned()
                .collect(),
        }
    }
}

/// What an undo did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undone {
    /// The rehearsal whose apply was taken back.
    pub rehearsal: String,
    /// When that apply happened, seconds since the Unix epoch.
    pub applied_at_unix: u64,
    /// The refs that were put back, stated in the direction the undo moved
    /// them: `before` is where the apply had left them, `after` is where they
    /// are now.
    pub restored: Vec<RefMove>,
    /// The branch whose worktree was reset, if the restored branch was the one
    /// checked out.
    pub reset: Option<String>,
    /// The record that was consumed.
    pub record: PathBuf,
}

/// Writes the record, and returns where it went.
///
/// `moved` is what the apply is about to do, not what it has done: this is
/// called before the transaction for exactly that reason. If the transaction
/// then fails, the record describes an apply that never landed — and an undo
/// reading it refuses, because none of the refs are where the record says the
/// apply left them, which is the honest answer.
///
/// # Errors
///
/// [`Error::Git`] if git cannot say where its own directory is, [`Error::Io`]
/// if the record cannot be written.
pub fn write(repo: &Path, record: &Record) -> Result<PathBuf> {
    let git_dir = PathBuf::from(git::run(repo, ["rev-parse", "--absolute-git-dir"])?);
    let path = git_dir.join(UNDO_FILE);
    fs::write(&path, render(record)).map_err(Error::io(&path))?;
    Ok(path)
}

/// Takes back the apply the record describes.
///
/// `id` is an optional unambiguous prefix of the rehearsal the caller means; it
/// is checked against the record rather than used to find one, because there is
/// only ever one record. See the module docs on being one level deep.
///
/// # Errors
///
/// [`Error::Refused`] if there is no record, if the record is not the apply
/// `id` names, if any ref has moved since that apply, or if the worktree is in
/// the way. [`Error::Git`] or [`Error::Io`] if the restore itself fails.
pub fn run(repo: &Path, id: Option<&str>) -> Result<Undone> {
    let git_dir = PathBuf::from(git::run(repo, ["rev-parse", "--absolute-git-dir"])?);
    let path = git_dir.join(UNDO_FILE);
    let text = fs::read_to_string(&path).map_err(|_| nothing_to_undo(&path))?;
    let record = parse(&text, &path)?;
    check_is_the_one_meant(&record, id)?;

    if record.refs.is_empty() {
        return Err(Error::Refused(format!(
            "the apply of rehearsal {} moved no branch, so there is nothing to put back.\n\
             The record is {}; delete it if it is in your way.",
            record.rehearsal,
            path.display()
        )));
    }

    let now = branches(repo)?;
    check_still_applied(&record, &now)?;

    let reset = match worktree_action(current_branch(repo).as_deref(), &record.refs) {
        Worktree::Refuse(branch) => return Err(refuse_deleting_the_checkout(&branch)),
        Worktree::Reset(branch) => {
            check_clean(repo)?;
            Some(branch)
        }
        Worktree::Untouched => None,
    };

    restore(repo, &record)?;

    if reset.is_some() {
        // Same as apply, for the same reason: the branch under HEAD now points
        // at the restored commit while the index and worktree still hold the
        // one the apply left. Resetting to HEAD, because HEAD is already right.
        git::run(repo, ["reset", "--hard", "--quiet"])?;
    }

    // Consumed only now, after everything that could refuse has refused: a
    // failed undo must leave the record where it was, or the way back is gone.
    fs::remove_file(&path).map_err(Error::io(&path))?;

    Ok(Undone {
        rehearsal: record.rehearsal,
        applied_at_unix: record.applied_at_unix,
        restored: record.refs.into_iter().map(reversed).collect(),
        reset,
        record: path,
    })
}

/// The record as it is written to disk.
///
/// Two properties are worth keeping, because between them they are what makes
/// this a file rather than a private blob:
///
/// 1. **Every line that is not a comment or a header field is a complete
///    `git update-ref` argument list.** Copy one, paste it after
///    `git update-ref`, and that ref goes back — with git verifying the value
///    it is replacing, so a hand restore is as safe as ours.
/// 2. **`HEAD` is not in it.** The pre-state records `HEAD` alongside the
///    branches, and it is left out here on purpose: `HEAD` is symbolic and
///    follows whichever branch is checked out, so restoring the branches
///    restores it, and a line saying `HEAD <sha> <sha>` would be the one line
///    in the file that hurts the person who uses it as the file invites —
///    `git update-ref HEAD …` detaches it.
#[must_use]
pub fn render(record: &Record) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# git-rehearse undo record — where `git rehearse apply` found this repository."
    );
    let _ = writeln!(text, "{VERSION_KEY} {RECORD_SCHEMA}");
    let _ = writeln!(text, "{REHEARSAL_KEY} {}", record.rehearsal);
    let _ = writeln!(text, "{APPLIED_AT_KEY} {}", record.applied_at_unix);
    let _ = writeln!(
        text,
        "#\n\
         # Each line below is a complete `git update-ref` argument list — put one ref\n\
         # back by hand with a copy and a paste:\n\
         #\n\
         #     git update-ref refs/heads/main <where it was> <where the apply left it>\n\
         #\n\
         # git refuses the update unless the ref is still where the apply left it, so a\n\
         # hand restore cannot quietly discard work committed since. An all-zero object\n\
         # name is git's own \"does not exist\": as the second field it deletes a branch\n\
         # the apply created, as the third it insists a branch the apply deleted is\n\
         # still absent.\n\
         #\n\
         # HEAD is not listed. It follows whichever branch is checked out, so restoring\n\
         # the branches restores it, and updating it directly would detach it."
    );
    for reference in &record.refs {
        let _ = writeln!(
            text,
            "{} {} {}",
            reference.name,
            reference.before.as_deref().unwrap_or(ABSENT),
            reference.after.as_deref().unwrap_or(ABSENT)
        );
    }
    text
}

/// Reads a record back.
///
/// Strict on purpose — principle 5. A record that cannot be read completely is
/// refused rather than half-applied, because the half that parsed is a set of
/// ref updates and there is no such thing as guessing at those.
///
/// `path` appears only in the messages; nothing is read from disk here.
///
/// # Errors
///
/// [`Error::Refused`] describing what about the file could not be read.
pub fn parse(text: &str, path: &Path) -> Result<Record> {
    // The version first and on its own, before anything else in the file is
    // interpreted: that is the whole point of having one. A later format may
    // spell every other line differently.
    let version = field(text, VERSION_KEY)
        .ok_or_else(|| malformed(path, "it has no `version` line"))?
        .parse::<u32>()
        .map_err(|_| malformed(path, "its `version` is not a number"))?;
    if version != RECORD_SCHEMA {
        return Err(malformed(
            path,
            &format!(
                "it uses record version {version} and this build understands \
                 {RECORD_SCHEMA} — upgrade git-rehearse, or restore the refs by hand"
            ),
        ));
    }

    let rehearsal = field(text, REHEARSAL_KEY)
        .ok_or_else(|| malformed(path, "it does not say which rehearsal it is from"))?
        .to_owned();
    let applied_at_unix = field(text, APPLIED_AT_KEY)
        .ok_or_else(|| malformed(path, "it does not say when it was applied"))?
        .parse::<u64>()
        .map_err(|_| malformed(path, "its `applied-at` is not a number"))?;

    let mut refs = Vec::new();
    for line in body(text) {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(before), Some(after), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(malformed(
                path,
                &format!("`{line}` is not a `<ref> <before> <after>` line"),
            ));
        };
        refs.push(RefMove {
            name: name.to_owned(),
            before: object(before),
            after: object(after),
        });
    }

    Ok(Record {
        rehearsal,
        applied_at_unix,
        refs,
    })
}

/// A header field's value.
///
/// Header keys are lowercase words, and a ref line always begins with a ref
/// name — `refs/…`, or `HEAD` if somebody has hand-added one. The two can
/// therefore never be confused, which is what lets the ref lines stay bare and
/// pasteable instead of carrying a keyword of their own.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| line.strip_prefix(key))
        .find_map(|rest| rest.strip_prefix(' '))
        .map(str::trim)
}

/// The lines that describe refs: everything that is neither blank, a comment,
/// nor a header field.
fn body(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| {
        !line.is_empty()
            && !line.starts_with('#')
            && ![VERSION_KEY, REHEARSAL_KEY, APPLIED_AT_KEY]
                .iter()
                .any(|key| line.starts_with(&format!("{key} ")))
    })
}

/// An object name, or `None` for the all-zero name that means "absent".
fn object(value: &str) -> Option<String> {
    (value != ABSENT).then(|| value.to_owned())
}

/// Refuses a record that is not the apply the caller had in mind.
///
/// The id is checked, never used to look one up: there is one record, and if it
/// is not this one then the apply the caller means is already unreachable —
/// saying so is the only useful thing left to do.
fn check_is_the_one_meant(record: &Record, id: Option<&str>) -> Result<()> {
    let Some(id) = id else { return Ok(()) };
    if record.rehearsal.starts_with(id) {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "the undo record is from the apply of rehearsal {}, not {id}.\n\
         Only the most recent apply can be undone: there is one record per repository, and \
         applying again overwrites it.",
        record.rehearsal
    )))
}

/// Refuses when the refs are not where the apply left them.
///
/// The same guarantee `apply` gives, pointed the other way, and stated in the
/// same shape so the two refusals read alike. Git checks it a second time
/// inside the transaction; this exists to produce a message a human can act on.
fn check_still_applied(record: &Record, now: &BTreeMap<String, String>) -> Result<()> {
    let mut differences: Vec<String> = Vec::new();
    for reference in &record.refs {
        match (now.get(&reference.name), &reference.after) {
            (Some(is), Some(left_at)) if is == left_at => {}
            (Some(is), Some(left_at)) => differences.push(format!(
                "  {} is now {is}, the apply left it at {left_at}",
                reference.name
            )),
            (Some(is), None) => differences.push(format!(
                "  {} is back at {is}, the apply deleted it",
                reference.name
            )),
            (None, Some(left_at)) => differences.push(format!(
                "  {} is gone, the apply left it at {left_at}",
                reference.name
            )),
            (None, None) => {}
        }
    }
    if differences.is_empty() {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "the repository has changed since the apply of rehearsal {}:\n{}\n\
         Undoing now would throw away whatever happened in between, so it is refused. Restore \
         the refs you actually want by hand — the record says where each one was.",
        record.rehearsal,
        differences.join("\n")
    )))
}

/// What an undo has to do about the worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Worktree {
    /// Nothing: the checked-out branch is not one the undo moves.
    Untouched,
    /// The checked-out branch is rewound, so the worktree follows it.
    Reset(String),
    /// The undo would delete the branch that is checked out.
    Refuse(String),
}

/// Which of those it is.
///
/// The deleting case is separated out rather than folded into the reset,
/// because there is nothing to reset *to*: undoing an apply that created the
/// branch you are standing on would leave `HEAD` pointing at a ref that no
/// longer exists — an unborn branch with a full worktree, which is a state a
/// user has to be talked out of rather than dropped into.
fn worktree_action(current: Option<&str>, refs: &[RefMove]) -> Worktree {
    let Some(branch) = current else {
        // A detached HEAD points at a commit, and undoing branch moves leaves
        // that commit exactly as valid as it was.
        return Worktree::Untouched;
    };
    let reference = format!("refs/heads/{branch}");
    let Some(moved) = refs.iter().find(|moved| moved.name == reference) else {
        return Worktree::Untouched;
    };
    match (&moved.before, &moved.after) {
        (None, _) => Worktree::Refuse(branch.to_owned()),
        (Some(before), Some(after)) if before == after => Worktree::Untouched,
        (Some(_), _) => Worktree::Reset(branch.to_owned()),
    }
}

fn refuse_deleting_the_checkout(branch: &str) -> Error {
    Error::Refused(format!(
        "undoing this apply would delete {branch}, which is the branch you have checked out.\n\
         Check out another branch first, then undo."
    ))
}

/// Refuses to rewind a worktree that has work in it.
///
/// Word for word the rule `apply` uses — tracked changes only, because
/// `git reset --hard` discards those and leaves untracked files alone.
fn check_clean(repo: &Path) -> Result<()> {
    let status = git::run(repo, ["status", "--porcelain", "--untracked-files=no"])?;
    if status.is_empty() {
        return Ok(());
    }
    Err(Error::Refused(
        "undoing this apply rewinds the branch you have checked out, and your worktree has \
         uncommitted changes that `git reset --hard` would destroy.\n\
         Commit or stash them first."
            .to_owned(),
    ))
}

/// The restore itself: one transaction, all or nothing.
fn restore(repo: &Path, record: &Record) -> Result<()> {
    let commands = restore_commands(&record.refs);
    if commands.is_empty() {
        return Ok(());
    }
    git::run_with_stdin(
        repo,
        [
            "update-ref",
            // The apply put a line in the reflog saying where the commits came
            // from; taking it back deserves a line of its own, or the branch
            // appears to have jumped backwards on its own.
            "-m",
            &format!("git-rehearse undo {}", record.rehearsal),
            "--stdin",
            "-z",
        ],
        Some(&commands),
    )?;
    Ok(())
}

/// The `update-ref --stdin -z` payload that reverses an apply.
///
/// Every command states the value it expects to replace — the value the apply
/// left — so git verifies the race check atomically inside the ref store, where
/// no window exists between checking and writing. `create` rather than an
/// update against an empty old value, for the same reason apply uses it: in
/// NUL-delimited mode an empty old value means "do not check", and a branch
/// somebody recreated in the meantime would be silently overwritten.
fn restore_commands(refs: &[RefMove]) -> String {
    let mut commands = String::new();
    for reference in refs {
        // [`Record::of_apply`] keeps HEAD out, so this can only be a
        // hand-edited record — and a hand-edited HEAD line is exactly the case
        // worth defending against, since acting on it would detach it.
        if reference.name == HEAD_KEY {
            continue;
        }
        match (&reference.before, &reference.after) {
            (Some(before), Some(after)) => {
                let _ = write!(commands, "update {}\0{before}\0{after}\0", reference.name);
            }
            // The apply created it, so putting things back means removing it.
            (None, Some(after)) => {
                let _ = write!(commands, "delete {}\0{after}\0", reference.name);
            }
            // The apply deleted it, so putting things back means bringing it
            // into existence again.
            (Some(before), None) => {
                let _ = write!(commands, "create {}\0{before}\0", reference.name);
            }
            (None, None) => {}
        }
    }
    commands
}

/// A ref move as the undo performed it, rather than as the apply did.
fn reversed(moved: RefMove) -> RefMove {
    RefMove {
        name: moved.name,
        before: moved.after,
        after: moved.before,
    }
}

/// The repository's branches, `name -> sha`.
///
/// Branches only. `HEAD` is not compared, because switching branches after an
/// apply invalidates nothing about undoing it: the branches are where the apply
/// left them either way, and refusing over the checkout would refuse a restore
/// that is still exactly correct. This is where undo and apply legitimately
/// differ — apply refuses a changed checkout because the *report* the user read
/// described the repository they were standing in.
fn branches(repo: &Path) -> Result<BTreeMap<String, String>> {
    git::refs(repo, "refs/heads/", 0)
}

/// The checked-out branch, or `None` for a detached `HEAD`.
fn current_branch(repo: &Path) -> Option<String> {
    git::run(repo, ["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()
}

fn nothing_to_undo(path: &Path) -> Error {
    Error::Refused(format!(
        "there is nothing to undo: no undo record at {}.\n\
         Only `git rehearse apply` writes one, and a successful undo uses it up.",
        path.display()
    ))
}

fn malformed(path: &Path, why: &str) -> Error {
    Error::Refused(format!(
        "{} is not a readable undo record: {why}.\n\
         Nothing has been changed. Restore the refs by hand, or delete the file.",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        ABSENT, RECORD_SCHEMA, Record, Worktree, check_is_the_one_meant, check_still_applied,
        parse, render, restore_commands, worktree_action,
    };
    use crate::analyze::RefMove;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn moved(name: &str, before: Option<&str>, after: Option<&str>) -> RefMove {
        RefMove {
            name: name.to_owned(),
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
        }
    }

    fn record() -> Record {
        Record {
            rehearsal: "1786248000-00".to_owned(),
            applied_at_unix: 1_786_248_000,
            refs: vec![
                moved("refs/heads/main", Some("aaa"), Some("bbb")),
                moved("refs/heads/spike", None, Some("ccc")),
                moved("refs/heads/gone", Some("ddd"), None),
            ],
        }
    }

    fn path() -> &'static Path {
        Path::new("/repo/.git/rehearse-undo")
    }

    fn state(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, sha)| ((*name).to_owned(), (*sha).to_owned()))
            .collect()
    }

    #[test]
    fn a_record_survives_a_round_trip_through_the_file() {
        assert_eq!(parse(&render(&record()), path()).expect("parses"), record());
    }

    #[test]
    fn every_ref_line_is_a_usable_update_ref_invocation() {
        // The property the format exists for: the second field is where the ref
        // goes, the third is what git must find there first. Both spellings of
        // "absent" are git's own, so a line stays a working command even when
        // one side of the move is a branch that does not exist.
        let text = render(&record());
        assert!(text.contains("\nrefs/heads/main aaa bbb\n"), "{text}");
        assert!(
            text.contains(&format!("\nrefs/heads/spike {ABSENT} ccc\n")),
            "{text}"
        );
        assert!(
            text.contains(&format!("\nrefs/heads/gone ddd {ABSENT}\n")),
            "{text}"
        );
    }

    #[test]
    fn the_record_says_which_version_which_rehearsal_and_when() {
        let text = render(&record());
        assert!(text.contains(&format!("version {RECORD_SCHEMA}")), "{text}");
        assert!(text.contains("rehearsal 1786248000-00"), "{text}");
        assert!(text.contains("applied-at 1786248000"), "{text}");
        // And a human is told what they are looking at before any of it.
        assert!(text.starts_with("# git-rehearse undo record"), "{text}");
    }

    #[test]
    fn a_record_from_another_version_is_refused_rather_than_half_read() {
        let text = render(&record()).replace("version 1", "version 2");
        let error = parse(&text, path()).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("record version 2"), "{message}");
        assert!(message.contains("upgrade git-rehearse"), "{message}");
        assert!(message.contains("rehearse-undo"), "{message}");
    }

    #[test]
    fn an_unreadable_record_names_the_line_it_choked_on() {
        let text = format!("{}refs/heads/half aaa\n", render(&record()));
        let error = parse(&text, path()).expect_err("refused");
        let message = error.to_string();
        assert!(message.contains("refs/heads/half aaa"), "{message}");
        assert!(
            message.contains("Nothing has been changed"),
            "a refusal has to say the repository is untouched: {message}"
        );
    }

    #[test]
    fn a_record_without_a_version_is_not_read_at_all() {
        // The pre-#58 format was `<sha> <name>` lines and a comment block. It
        // has no version line, so it lands here rather than being mistaken for
        // a current record with the fields in a different order.
        let error = parse("# git-rehearse undo record\naaa refs/heads/main\n", path())
            .expect_err("refused");
        assert!(error.to_string().contains("no `version` line"), "{error}");
    }

    #[test]
    fn the_transaction_reverses_every_kind_of_move() {
        let commands = restore_commands(&record().refs);
        assert!(
            commands.contains("update refs/heads/main\0aaa\0bbb\0"),
            "a move goes back, stating what the apply left: {commands:?}"
        );
        assert!(
            commands.contains("delete refs/heads/spike\0ccc\0"),
            "a branch the apply created is removed: {commands:?}"
        );
        assert!(
            commands.contains("create refs/heads/gone\0ddd\0"),
            "a branch the apply deleted comes back: {commands:?}"
        );
    }

    #[test]
    fn head_is_never_updated_directly() {
        // A record does not contain it, and if a hand-edited one does, acting
        // on it would detach HEAD rather than restore anything.
        assert!(
            !Record::of_apply(
                "id".to_owned(),
                0,
                &[
                    moved("HEAD", Some("aaa"), Some("bbb")),
                    moved("refs/heads/main", Some("aaa"), Some("bbb")),
                ],
            )
            .refs
            .iter()
            .any(|reference| reference.name == "HEAD")
        );
        let commands = restore_commands(&[moved("HEAD", Some("aaa"), Some("bbb"))]);
        assert!(commands.is_empty(), "{commands:?}");
    }

    #[test]
    fn a_repository_still_where_the_apply_left_it_passes() {
        let now = state(&[("refs/heads/main", "bbb"), ("refs/heads/spike", "ccc")]);
        assert!(check_still_applied(&record(), &now).is_ok());
    }

    #[test]
    fn a_commit_since_the_apply_is_named_with_both_values() {
        let now = state(&[("refs/heads/main", "eee"), ("refs/heads/spike", "ccc")]);

        let error = check_still_applied(&record(), &now).expect_err("refused");

        let message = error.to_string();
        assert!(
            message.contains("refs/heads/main is now eee, the apply left it at bbb"),
            "{message}"
        );
        assert!(
            message.contains("throw away whatever happened"),
            "{message}"
        );
    }

    #[test]
    fn a_ref_deleted_or_recreated_since_the_apply_is_also_a_change() {
        // The branch the apply moved has been deleted since.
        let gone = state(&[("refs/heads/spike", "ccc")]);
        let error = check_still_applied(&record(), &gone).expect_err("refused");
        assert!(error.to_string().contains("is gone"), "{error}");

        // And the branch the apply deleted has been recreated since.
        let back = state(&[
            ("refs/heads/main", "bbb"),
            ("refs/heads/spike", "ccc"),
            ("refs/heads/gone", "fff"),
        ]);
        let error = check_still_applied(&record(), &back).expect_err("refused");
        assert!(
            error.to_string().contains("refs/heads/gone is back at fff"),
            "{error}"
        );
    }

    #[test]
    fn the_worktree_is_reset_only_when_the_checked_out_branch_is_rewound() {
        assert_eq!(
            worktree_action(Some("main"), &record().refs),
            Worktree::Reset("main".to_owned())
        );
        assert_eq!(
            worktree_action(Some("other"), &record().refs),
            Worktree::Untouched,
            "another branch going back leaves this worktree alone"
        );
        assert_eq!(
            worktree_action(None, &record().refs),
            Worktree::Untouched,
            "a detached HEAD keeps pointing at a commit that is still valid"
        );
    }

    #[test]
    fn undoing_into_a_branch_that_would_be_deleted_is_refused_not_reset() {
        // The apply created `spike` and the user has since checked it out.
        // There is no commit to reset to, and deleting the ref under HEAD would
        // leave an unborn branch over a full worktree.
        assert_eq!(
            worktree_action(Some("spike"), &record().refs),
            Worktree::Refuse("spike".to_owned())
        );
    }

    #[test]
    fn undoing_a_different_apply_than_the_one_meant_is_refused() {
        let error = check_is_the_one_meant(&record(), Some("1786249999")).expect_err("refused");
        let message = error.to_string();
        assert!(
            message.contains("from the apply of rehearsal 1786248000-00"),
            "{message}"
        );
        assert!(
            message.contains("applying again overwrites it"),
            "the one-level-deep property has to be said out loud: {message}"
        );

        // A prefix of the recorded id is how every other command takes an id.
        assert!(check_is_the_one_meant(&record(), Some("17862480")).is_ok());
        assert!(check_is_the_one_meant(&record(), None).is_ok());
    }
}
