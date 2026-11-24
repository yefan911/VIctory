//! Repository maintenance. Run with `cargo xtask <command>`.

mod policy;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!(
                "hooks installed: commits, merges and `git am` now run `cargo xtask policy --staged`"
            );
            Ok(())
        }
    }
}

/// The workspace `cargo xtask` was run in.
///
/// Cargo sets `CARGO_MANIFEST_DIR` for the process `cargo run` and
/// `cargo test` start, naming the tree the command was run in. The value
/// compiled in names the tree the binary was built from, and the two differ
/// when worktrees share a `CARGO_TARGET_DIR`: the xtask packages there have
/// the same name and version, so cargo takes one worktree's build as fresh
/// for another and runs it. Its pre-commit hook then checked the other
/// worktree, and passed whatever this one staged.
fn workspace_root() -> PathBuf {
    workspace_root_from(std::env::var_os("CARGO_MANIFEST_DIR"))
}

/// The parent of `manifest_dir` when it is an xtask package directory, else
/// that of the directory this binary was built from: a binary started some
/// other way may inherit the variable from whatever cargo command ran above
/// it, naming another package.
fn workspace_root_from(manifest_dir: Option<std::ffi::OsString>) -> PathBuf {
    let built = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest_dir
        .map(PathBuf::from)
        .filter(|dir| {
            dir.is_absolute()
                && dir.file_name() == built.file_name()
                && dir.join("Cargo.toml").is_file()
        })
        .unwrap_or_else(|| built.to_path_buf());
    dir.parent().expect("xtask lives in the workspace").into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `cargo xtask` run in another worktree, sharing this one's
    /// `CARGO_TARGET_DIR`, runs this binary with that worktree's
    /// `CARGO_MANIFEST_DIR`; the check must read that worktree.
    #[test]
    fn workspace_root_is_where_cargo_ran() {
        let built = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("worktree");
        let xtask = other.join("xtask");
        std::fs::create_dir_all(&xtask).unwrap();
        std::fs::write(xtask.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(workspace_root_from(Some(xtask.clone().into())), other);

        assert_eq!(workspace_root_from(None), built);
        // Some other package's variable, inherited by a binary run directly.
        let core = other.join("crates/victory-core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::write(core.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(workspace_root_from(Some(core.into())), built);
        // No package there, or not a path cargo would set.
        assert_eq!(
            workspace_root_from(Some(tmp.path().join("gone/xtask").into())),
            built
        );
        assert_eq!(workspace_root_from(Some("xtask".into())), built);
    }

    /// Git on Linux and macOS skips a hook that is not executable (with only
    /// a hint), so the policy check would silently never run there.
    #[test]
    fn hooks_are_tracked_executable() {
        let out = policy::git_command(&workspace_root())
            .args(["ls-files", "--stage", "--", ".githooks"])
            .output();
        let Some(out) = out.ok().filter(|o| o.status.success()) else {
            eprintln!("skipped: not a git checkout");
            return;
        };
        let listing = String::from_utf8(out.stdout).unwrap();
        assert!(!listing.is_empty(), ".githooks/ holds no tracked hooks");
        for line in listing.lines() {
            assert!(
                line.starts_with("100755 "),
                "hook not executable in git (git update-index --chmod=+x): {line}"
            );
        }
    }
}
