//! The `marsh split apply --json` script, run with the host `sh` and `git`
//! on throwaway trees: it reads the outcome from the tree and the index.
//! The shell-VM run of the same script is `tests/acceptance/lanes_uat.py`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCRIPT: &str = include_str!("../src/split_apply.sh");

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn seed(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("a.txt"), "l1\nl2\nl3\nl4\n").unwrap();
    fs::write(dir.join("b.txt"), "k\n").unwrap();
    fs::write(dir.join("gone.txt"), "x\n").unwrap();
}

/// A root and a patch (a lane's edit of `a.txt`, `b.txt` and `gone.txt`),
/// then `prepare` shapes the root. Returns the script's reply and the root.
fn run(repo: bool, prepare: impl FnOnce(&Path)) -> (Vec<String>, PathBuf) {
    let scratch = tempfile::tempdir().unwrap().keep();
    let (root, lane) = (scratch.join("root"), scratch.join("lane"));
    seed(&root);
    seed(&lane);
    if repo {
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "i"]);
    }
    git(&lane, &["init", "-q"]);
    git(&lane, &["add", "."]);
    git(&lane, &["commit", "-qm", "i"]);
    fs::write(lane.join("a.txt"), "l1\nLANE\nl3\nl4\n").unwrap();
    fs::write(lane.join("b.txt"), "k2\n").unwrap();
    fs::write(lane.join("gone.txt"), "x2\n").unwrap();
    let diff = Command::new("git")
        .args(["diff", "--binary"])
        .current_dir(&lane)
        .output()
        .unwrap();
    let patch = scratch.join("p.patch");
    fs::write(&patch, diff.stdout).unwrap();
    let files = scratch.join("files");
    fs::write(&files, "M\ta.txt\nM\tb.txt\nM\tgone.txt\n").unwrap();
    prepare(&root);
    let output = Command::new("sh")
        .arg("-c")
        .arg(SCRIPT)
        .arg("apply")
        .arg(&root)
        .arg("")
        .arg(&patch)
        .arg(&files)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let words = output
        .stdout
        .split(|byte| *byte == 0)
        .map(|word| String::from_utf8_lossy(word).into_owned())
        .collect();
    (words, root)
}

#[test]
fn a_patch_that_applies_is_clean_and_writes_the_tree() {
    if !have_git() {
        return;
    }
    for repo in [true, false] {
        let (words, root) = run(repo, |_| {});
        assert_eq!(&words[..2], ["R", "clean"], "{words:?}");
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "k2\n");
    }
}

#[test]
fn a_root_edit_of_the_same_lines_is_conflicts_with_markers_and_paths() {
    if !have_git() {
        return;
    }
    let (words, root) = run(true, |root| {
        fs::write(root.join("a.txt"), "l1\nROOT\nl3\nl4\n").unwrap();
        git(root, &["commit", "-qam", "r"]);
    });
    assert_eq!(&words[..2], ["R", "conflicts"], "{words:?}");
    assert!(
        words.windows(2).any(|pair| pair == ["C", "a.txt"]),
        "{words:?}"
    );
    let merged = fs::read_to_string(root.join("a.txt")).unwrap();
    assert!(merged.contains("<<<<<<<") && merged.contains("ROOT") && merged.contains("LANE"));
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "k2\n");
}

#[test]
fn a_patch_touching_a_file_deleted_in_the_root_fails_and_changes_nothing() {
    if !have_git() {
        return;
    }
    for repo in [true, false] {
        let (words, root) = run(repo, |root| {
            fs::remove_file(root.join("gone.txt")).unwrap();
            if repo {
                git(root, &["commit", "-qam", "r"]);
            }
        });
        assert_eq!(&words[..2], ["R", "failed"], "{words:?}");
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "l1\nl2\nl3\nl4\n"
        );
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "k\n");
        assert!(!root.join("gone.txt").exists());
        if repo {
            let status = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(status.stdout.is_empty(), "{status:?}");
        }
    }
}

#[test]
fn a_three_way_apply_leaves_no_index_entry_that_needs_the_split_objects() {
    if !have_git() {
        return;
    }
    // The lane's blobs live only in an alternate object store, as a split's
    // `store.git` does; after the apply the index must not depend on it.
    let scratch = tempfile::tempdir().unwrap().keep();
    let (root, lane, store) = (
        scratch.join("root"),
        scratch.join("lane"),
        scratch.join("store"),
    );
    seed(&root);
    seed(&lane);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "i"]);
    git(&lane, &["init", "-q"]);
    git(&lane, &["add", "."]);
    git(&lane, &["commit", "-qm", "i"]);
    fs::write(lane.join("b.txt"), "k2\n").unwrap();
    git(&lane, &["add", "b.txt"]);
    let diff = Command::new("git")
        .args(["diff", "--cached", "--binary"])
        .current_dir(&lane)
        .output()
        .unwrap();
    // Move the lane's objects out of its repo into the "store".
    fs::rename(lane.join(".git/objects"), &store).unwrap();
    // A root edit elsewhere forces the 3-way path (the plain apply still
    // works, so also touch the line the patch changes).
    fs::write(root.join("b.txt"), "k-root\n").unwrap();
    git(&root, &["commit", "-qam", "r"]);
    let patch = scratch.join("p.patch");
    fs::write(&patch, diff.stdout).unwrap();
    let files = scratch.join("files");
    fs::write(&files, "M\tb.txt\n").unwrap();
    let output = Command::new("sh")
        .arg("-c")
        .arg(SCRIPT)
        .arg("apply")
        .arg(&root)
        .arg(&store)
        .arg(&patch)
        .arg(&files)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let reply = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(reply.starts_with("R\0conflicts\0"), "{reply:?}");
    // Without the alternate, every index entry's object exists.
    let fsck = Command::new("git")
        .args(["fsck", "--no-dangling"])
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        fsck.status.success() && !String::from_utf8_lossy(&fsck.stderr).contains("missing"),
        "{fsck:?}"
    );
}
