//! Rule #1, statically: the repository only ever contains small text files
//! that we wrote. See docs/asset-policy.md.
//!
//! This is an allowlist, not a blocklist: a file passes only if its name is
//! on the list, it is not too big, it is text, and it does not smuggle data
//! inside text (long hex/base64 runs).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const MAX_FILE_SIZE: usize = 512 * 1024;
const MAX_ENCODED_RUN: usize = 512;
/// Longest array of numeric literals, or string of `\xHH` escapes, a file may
/// hold. The largest table in the tree (XMA's, 272 entries) is well under.
const MAX_LITERAL_RUN: usize = 512;

const ALLOWED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli", "ps1", "cmd", "sh",
];
const ALLOWED_NAMES: &[&str] = &[
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".victory-root",
    "pre-commit",
    "pre-merge-commit",
    "pre-applypatch",
    "LICENSE",
    "LICENSE-MIT",
    "LICENSE-APACHE",
];

/// Paths explicitly exempted, each pinned to the BLAKE3 of the one content
/// it is exempted for, with a reason. Keep this empty if at all possible.
const EXCEPTIONS: &[(&str, &str, &str)] = &[(
    "crates/victory-formats/src/bcpack-tables.bin",
    "f0f032aa310e7cfd431c075d2816decdd39d21b1927cbffc38386599c0de73c0",
    "the BCPack format's constant tables: nothing decodes without them and they cannot be derived (owner's decision, 2026-11-23; docs/asset-policy.md)",
)];

/// Signatures of formats that must never be committed.
const FORBIDDEN_MAGIC: &[(&[u8], &str)] = &[
    (b"MZ", "PE executable"),
    (b"DXBC", "shader container"),
    (b"DDS ", "DDS texture"),
    (b"RIFF", "RIFF (audio/video)"),
    (b"BC\xC0\xDE", "LLVM bitcode"),
    (b"Microsoft C/C++ MSF", "PDB"),
    (b"!<arch>", "library archive"),
    (b"PK\x03\x04", "zip"),
    (b"7z\xBC\xAF", "7-zip"),
    (b"MSCF", "cabinet"),
    (b"\xD0\xCF\x11\xE0", "MSI/OLE"),
];

/// First line of a Git LFS pointer, current and pre-1.0 spec. The file
/// itself is small text; what it stands for is stored outside the
/// repository, where this check never sees it.
const LFS_POINTER_PREFIXES: &[&[u8]] = &[
    b"version https://git-lfs.github.com/spec/",
    b"version https://hawser.github.com/spec/",
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let files = list_files(root, staged)?;
    let indexed = indexed_blobs(root, &files)?;
    let mut violations = Vec::new();
    for file in &files {
        let path = &file.name;
        if !file.utf8_name {
            // Lossily decoded, so it could not be matched or read by name.
            violations.push(format!("{path}: file name is not valid UTF-8"));
        }
        let content = match read(root, file) {
            Ok(Some(content)) => content,
            Ok(None) => continue,
            Err(problem) => {
                violations.push(format!("{path}: {problem}"));
                continue;
            }
        };
        check_content(path, path, &content, &mut violations);
        // The working copy is not what the repository holds: a tracked file
        // edited after a `--no-verify` commit, or hidden from `git status`
        // by `--assume-unchanged` or `--skip-worktree`, still has its
        // committed content in the index. Checked as well when it differs.
        // A conflicted path has an entry per stage (base, ours, theirs),
        // and each is checked: `git checkout --ours` or `--theirs` puts any
        // one of them in the next commit.
        if let Source::Worktree { index, .. } = &file.source {
            for oid in index {
                match indexed.get(oid) {
                    Some(Some(blob)) if *blob != content => {
                        check_content(path, &format!("{path} (as indexed)"), blob, &mut violations);
                    }
                    Some(Some(_)) => {}
                    _ => violations.push(format!("{path}: cannot read the indexed content")),
                }
            }
        }
    }
    if violations.is_empty() {
        println!("asset policy: {} files ok", files.len());
        return Ok(());
    }
    for v in &violations {
        eprintln!("POLICY: {v}");
    }
    bail!(
        "{} asset-policy violation(s). Nothing from the game may be committed; \
         see docs/asset-policy.md",
        violations.len()
    )
}

/// Adds the problems with one file's content, reported as `label`.
fn check_content(path: &str, label: &str, content: &Content, violations: &mut Vec<String>) {
    if content.size > content.head.len() as u64 {
        // Only the start was read: no exception covers it, since the one
        // exempted content is within the limit, and the start shows the rest.
        violations.push(format!(
            "{label}: {} bytes exceeds the {MAX_FILE_SIZE}-byte limit",
            content.size
        ));
        for problem in check(path, content) {
            violations.push(format!("{label}: {problem}"));
        }
        return;
    }
    if let Some(problem) = exempt(path, content) {
        if let Err(problem) = problem {
            violations.push(format!("{label}: {problem}"));
        }
        return;
    }
    for problem in check(path, content) {
        violations.push(format!("{label}: {problem}"));
    }
}

/// Content as read: at most the first `MAX_FILE_SIZE` bytes, and the size of
/// the whole.
///
/// A file over the limit is refused whatever it holds, and is read no
/// further: a game dump or disc image of several gigabytes left in the
/// checkout (untracked, not ignored) or staged by mistake was read into
/// memory whole before being refused as too big, and could exhaust it.
/// The rest is still streamed through, without being kept, to count it.
#[derive(Debug, PartialEq)]
struct Content {
    head: Vec<u8>,
    size: u64,
}

impl std::ops::Deref for Content {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.head
    }
}

impl Content {
    fn read(reader: impl std::io::Read) -> std::io::Result<Self> {
        use std::io::Read;
        let mut reader = reader;
        let mut head = Vec::new();
        (&mut reader)
            .take(MAX_FILE_SIZE as u64)
            .read_to_end(&mut head)?;
        let rest = std::io::copy(&mut reader, &mut std::io::sink())?;
        Ok(Self {
            size: head.len() as u64 + rest,
            head,
        })
    }
}

/// The index's blob for every tracked file of the full check, by object id,
/// read with one `git cat-file --batch` rather than a process per file;
/// `None` for an object git does not have.
fn indexed_blobs(
    root: &Path,
    files: &[File],
) -> Result<std::collections::HashMap<String, Option<Content>>> {
    use std::io::{BufRead, Read, Write};
    let mut oids: Vec<&str> = files
        .iter()
        .filter_map(|file| match &file.source {
            Source::Worktree { index, .. } => Some(index.iter().map(String::as_str)),
            Source::Index { .. } => None,
        })
        .flatten()
        .collect();
    oids.sort_unstable();
    oids.dedup();
    let mut blobs = std::collections::HashMap::new();
    if oids.is_empty() {
        return Ok(blobs);
    }
    let mut child = git_command(root)
        .args(["cat-file", "--batch"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("running git cat-file")?;
    let mut stdin = child.stdin.take().context("git cat-file stdin")?;
    let request: String = oids.iter().map(|oid| format!("{oid}\n")).collect();
    // Written from another thread: git answers while it reads, and a full
    // stdout pipe would otherwise stop both.
    let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
    let mut out = std::io::BufReader::new(child.stdout.take().context("git cat-file stdout")?);
    for oid in &oids {
        // `<oid> <type> <size>\n<content>\n`, or `<oid> missing\n`.
        let mut header = String::new();
        out.read_line(&mut header).context("reading git cat-file")?;
        let size = match header.split_whitespace().collect::<Vec<_>>()[..] {
            [_, _, size] => size.parse::<u64>().ok(),
            _ => None,
        };
        let Some(size) = size else {
            if header.is_empty() {
                bail!("git cat-file ended early");
            }
            blobs.insert((*oid).to_owned(), None);
            continue;
        };
        let content = Content::read((&mut out).take(size)).context("reading git cat-file")?;
        if content.size != size {
            bail!("git cat-file ended early");
        }
        out.read_exact(&mut [0]).context("reading git cat-file")?;
        blobs.insert((*oid).to_owned(), Some(content));
    }
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("git cat-file writer panicked"))?
        .context("writing to git cat-file")?;
    child.wait().context("waiting for git cat-file")?;
    Ok(blobs)
}

/// One path to check.
struct File {
    /// Repository-relative, `/`-separated, lossily decoded if need be.
    name: String,
    utf8_name: bool,
    source: Source,
}

enum Source {
    /// On disk, at `path`. `index` is the object ids of the blobs the index
    /// holds for it, one per stage, none if untracked: the content that
    /// stays in the repository, and in the next commit, while the file is
    /// missing from disk.
    Worktree { path: PathBuf, index: Vec<String> },
    /// Staged: the index entry's mode and object id.
    Index { mode: String, oid: String },
}

/// The content to check; `Ok(None)` if there is nothing to check (a file
/// gone from both the working tree and the index), `Err` with the problem if
/// a path the check cannot read must not pass unread.
fn read(root: &Path, file: &File) -> Result<Option<Content>, String> {
    match &file.source {
        Source::Index { mode, .. } if mode == "160000" => {
            Err("a submodule: content from another repository bypasses this check".to_owned())
        }
        Source::Index { oid, .. } => read_blob(root, oid).map(Some),
        Source::Worktree { path, index } => {
            // Not `metadata`: that follows a symlink, and git stores a
            // symlink's target path, not what it points to.
            let kind = match std::fs::symlink_metadata(path) {
                Ok(meta) => meta.file_type(),
                // Deleted but not `git rm`ed, or left out of a sparse
                // checkout: the index still holds the file, and a plain
                // `git commit` keeps it. Committed with `--no-verify` and
                // then removed, it used to pass the full check unread.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // `run` checks the other stages against this one.
                    return index.first().map(|oid| read_blob(root, oid)).transpose();
                }
                Err(e) => return Err(format!("cannot read: {e}")),
            };
            if kind.is_symlink() {
                let target = std::fs::read_link(path).map_err(|e| format!("cannot read: {e}"))?;
                let head = link_bytes(&target);
                let size = head.len() as u64;
                return Ok(Some(Content { head, size }));
            }
            if kind.is_dir() {
                // `git ls-files --others` lists an untracked repository
                // inside this one as its directory, with a trailing `/`.
                return Err(if file.name.ends_with('/') {
                    "a git repository nested in this one: its files bypass this check"
                } else {
                    "a directory tracked as a file (a submodule?)"
                }
                .to_owned());
            }
            if !kind.is_file() {
                // A FIFO or device: reading it could block or never end.
                return Err("not a regular file".to_owned());
            }
            std::fs::File::open(path)
                .and_then(Content::read)
                .map(Some)
                .map_err(|e| format!("cannot read: {e}"))
        }
    }
}

/// A blob's content. By object id, not `:<path>`, so no file name can make
/// it fail.
/// Streamed, so a blob over the limit is not held in memory whole.
fn read_blob(root: &Path, oid: &str) -> Result<Content, String> {
    let mut child = git_command(root)
        .args(["cat-file", "blob", oid])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let content = child.stdout.take().map(Content::read);
    let status = child.wait().map_err(|e| format!("cannot run git: {e}"))?;
    match content {
        Some(Ok(content)) if status.success() => Ok(content),
        _ => Err("cannot read the indexed content".to_owned()),
    }
}

/// A symlink's content as git stores it: the target path.
fn link_bytes(target: &Path) -> Vec<u8> {
    #[cfg(unix)]
    let bytes =
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::as_bytes(target.as_os_str()).to_vec();
    // Git for Windows stores the target as UTF-8 with `/` separators.
    #[cfg(not(unix))]
    let bytes = target.to_string_lossy().replace('\\', "/").into_bytes();
    bytes
}

/// Variables through which the environment, not `root`, picks the repository
/// git works on (`git rev-parse --local-env-vars`).
#[cfg(test)]
const REPOSITORY_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// Variables that change how git reads a pathspec: literal, glob or
/// case-insensitive. Tests pass pathspecs (`check-ignore -- <path>`, the
/// hooks' `ls-files -- .githooks`), and `check-ignore` refuses to run at all
/// under `GIT_LITERAL_PATHSPECS`, so `tempdir` would take an ignored
/// directory for a visible one.
#[cfg(test)]
const PATHSPEC_ENV: &[&str] = &[
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// `git`, run in `root`.
///
/// The hook needs the environment it inherits: `git commit -a` and
/// `git commit <path>` stage into a temporary index that only
/// `GIT_INDEX_FILE` names. Tests point `root` at temporary repositories
/// instead, and when they run from a hook those variables would send every
/// one of them to the same, outside index, so there they are dropped.
///
/// Temporary repositories also read no global or system configuration: a
/// developer's `core.excludesFile` would hide the files a test stages, and
/// their `core.hooksPath` would run their hooks inside it. Nor do they read
/// the ignore and attributes files git looks for without any configuration
/// (`$XDG_CONFIG_HOME/git/ignore`, else `~/.config/git/ignore`, the same for
/// `attributes`, and the system gitattributes): a `*.rs` there hides what a
/// test stages just as an excludes file would, and a `filter=` there changes
/// what it stages. The checkout itself keeps all of that, since a global
/// excludes file is how files that will never be added (`.DS_Store`) stay
/// out of the full check.
///
/// Nor are they made from the environment's choices: `GIT_TEMPLATE_DIR`
/// copies its `info/exclude` and hooks into every `git init`, and
/// `GIT_DEFAULT_HASH=sha256` makes repositories that the SHA-1 object ids
/// tests stage are refused by. `GIT_ATTR_SOURCE` reads attributes from a
/// tree-ish that a new repository does not have, and every command fails.
/// Nor do they look for a repository above themselves.
///
/// Nor does a hook's quarantine reach them: a `pre-receive` hook runs with
/// `GIT_QUARANTINE_PATH` set beside the object directories dropped above,
/// and with it git refuses every ref update, `git init` included. And since
/// they read no global configuration, the `safe.directory` a developer
/// keeps there for a TMPDIR owned by someone else (a shared one, or one an
/// elevated Windows shell made, which belongs to Administrators) cannot
/// reach them either; they trust themselves, which a test made them, as
/// command-line configuration that git honours for it.
pub(crate) fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    // `git replace` refs are local and never pushed, yet git reads through
    // them by default: replacing a staged blob shows `cat-file` other
    // content than the commit will hold, and replacing HEAD with a commit
    // whose tree already has a staged file drops it from `diff --cached`.
    // The check reads the objects as they will be committed.
    command.current_dir(root).arg("--no-replace-objects");
    unredirect_output(&mut command, std::env::vars_os().map(|(key, _)| key));
    #[cfg(test)]
    {
        for var in REPOSITORY_ENV.iter().chain(PATHSPEC_ENV) {
            command.env_remove(var);
        }
        if !is_checkout(root) {
            // Git for Windows opens "/dev/null" as NUL.
            command
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_ATTR_NOSYSTEM", "1")
                .env("XDG_CONFIG_HOME", NO_XDG_CONFIG)
                // Empty: `git init` copies no template at all, silently.
                .env("GIT_TEMPLATE_DIR", "")
                .env_remove("GIT_DEFAULT_HASH")
                // Reads `.gitattributes` from that tree-ish instead of the
                // working tree; a fresh repository has no HEAD to read.
                .env_remove("GIT_ATTR_SOURCE")
                .env_remove("GIT_QUARANTINE_PATH")
                .args(["-c", "safe.directory=*"]);
            // Git looks no further up than `root` itself: a directory that is
            // not a repository is walked, not taken for part of whatever
            // repository encloses it (the checkout, when the temporary
            // directory is made beside the test binary). Git stops before
            // entering a ceiling, so the ceiling is the parent.
            #[cfg(unix)]
            if let Some(parent) = root.is_absolute().then(|| root.parent()).flatten() {
                command.env("GIT_CEILING_DIRECTORIES", parent);
            }
        }
    }
    command
}

/// Whether `root` is the checkout itself, however it is spelled.
///
/// `tempdir` canonicalizes the checkout before asking `check-ignore` about
/// a TMPDIR inside it. Reached through a symlink (`~/src` pointing at
/// another disk), or with `..` in `CARGO_MANIFEST_DIR`, the canonical path
/// differed from `workspace_root`, so the checkout was taken for a
/// temporary repository and read without the developer's global excludes
/// file: a TMPDIR ignored only there, which the full check never lists,
/// was refused, and with no other candidate every test panicked.
#[cfg(test)]
fn is_checkout(root: &Path) -> bool {
    let checkout = crate::workspace_root();
    root == checkout
        || matches!(
            (root.canonicalize(), checkout.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Keeps git's stdout to what the command prints, since the check parses it.
///
/// `GIT_TRACE=/dev/stdout` (or `GIT_TRACE_SETUP`, `GIT_TRACE2_EVENT`, any
/// of the trace variables pointed at stdout) puts trace lines in front of
/// the first NUL-separated field. In the full check that field is the first
/// file listed: its name no longer exists, so it is never read, and that
/// file passes whatever it holds. `--staged` stops at a record it cannot
/// parse, and every test that reads git's output failed. Git for Windows'
/// `GIT_REDIRECT_STDOUT` sends the output to a file instead, and the check
/// sees an empty repository. Trace2 can also be turned on from global or
/// system configuration, which its variables override, so those are set
/// off rather than only dropped.
fn unredirect_output(command: &mut Command, keys: impl IntoIterator<Item = std::ffi::OsString>) {
    for key in keys {
        // Variable names are case-insensitive on Windows.
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_TRACE") || upper.starts_with("GIT_REDIRECT_") {
            command.env_remove(key);
        }
    }
    for var in ["GIT_TRACE2", "GIT_TRACE2_EVENT", "GIT_TRACE2_PERF"] {
        command.env(var, "0");
    }
}

/// A configuration directory that never exists, so git finds no `git/ignore`
/// or `git/attributes` in it. Relative paths would resolve against each
/// temporary repository, where a test could create them.
#[cfg(test)]
const NO_XDG_CONFIG: &str = if cfg!(windows) {
    "NUL:/victory-policy-tests"
} else {
    "/dev/null/victory-policy-tests"
};

/// Splits NUL-terminated git output; each field is returned raw.
fn git_z(root: &Path, args: &[&str]) -> Result<Option<Vec<Vec<u8>>>> {
    let out = git_command(root)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    ))
}

/// Decodes a path git printed, keeping the exact bytes for reading it.
///
/// Git separates directories with `/` everywhere. On Unix a `\` is an
/// ordinary byte of a name, so it is kept: shown as `/`, `odd\f.rs` was
/// listed as `odd/f.rs`, a file in a directory that does not exist, and a
/// name such as `assets\x.rs` read as a path through `assets/`.
fn decode(root: &Path, raw: &[u8]) -> (String, bool, PathBuf) {
    let name = String::from_utf8_lossy(raw);
    #[cfg(not(unix))]
    let name = name.replace('\\', "/");
    #[cfg(unix)]
    let name = name.into_owned();
    let utf8 = std::str::from_utf8(raw).is_ok();
    #[cfg(unix)]
    let on_disk = root.join(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(raw));
    // Git for Windows prints paths as UTF-8, and Windows names are Unicode.
    #[cfg(not(unix))]
    let on_disk = root.join(&name);
    (name, utf8, on_disk)
}

fn list_files(root: &Path, staged: bool) -> Result<Vec<File>> {
    list_files_built_by(root, staged, std::env::current_exe().ok().as_deref())
}

/// `list_files`, passing over the untracked files of the build directory of
/// `exe` (see `build_dir_in`).
fn list_files_built_by(root: &Path, staged: bool, exe: Option<&Path>) -> Result<Vec<File>> {
    if staged {
        return staged_files(root);
    }
    if let Some(fields) = git_z(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )? {
        // A submodule is a directory on disk, or nothing if not checked out;
        // only the index says what it is. Each entry is
        // `<mode> <oid> <stage>\t<path>`; a conflicted path has several,
        // and any one of them is content the check must see.
        let mut index: std::collections::HashMap<Vec<u8>, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for entry in git_z(root, &["ls-files", "-z", "--stage"])?.unwrap_or_default() {
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let Ok(header) = std::str::from_utf8(&entry[..tab]) else {
                continue;
            };
            let mut parts = header.split(' ');
            if let (Some(mode), Some(oid)) = (parts.next(), parts.next()) {
                index
                    .entry(entry[tab + 1..].to_vec())
                    .or_default()
                    .push((mode.to_owned(), oid.to_owned()));
            }
        }
        let build = exe.and_then(|exe| build_dir_in(root, exe));
        let mut files: Vec<File> = fields
            .iter()
            .filter(|raw| index.contains_key(*raw) || !is_under(raw, build.as_deref()))
            .map(|raw| (raw, decode(root, raw)))
            .filter(|(raw, (_, _, on_disk))| {
                index.contains_key(*raw) || !exe.is_some_and(|exe| leads_to_build_dir(on_disk, exe))
            })
            .map(|(raw, (name, utf8_name, on_disk))| {
                let stages = index.get(raw).map(Vec::as_slice).unwrap_or_default();
                let source = if stages.iter().any(|(mode, _)| mode == "160000") {
                    Source::Index {
                        mode: "160000".to_owned(),
                        oid: String::new(),
                    }
                } else {
                    Source::Worktree {
                        path: on_disk,
                        index: stages.iter().map(|(_, oid)| oid.clone()).collect(),
                    }
                };
                File {
                    name,
                    utf8_name,
                    source,
                }
            })
            .collect();
        files.sort_by(|a, b| a.name.cmp(&b.name));
        files.dedup_by(|a, b| a.name == b.name);
        return Ok(files);
    }
    // Not a git repository: walk everything except build output.
    //
    // Every entry that is not a directory is listed, as git would list it:
    // a symlink is read as the target path git would store, and a FIFO or
    // device is refused unread. Only regular files used to be listed, so
    // in an unpacked source tree a symlink named `game.xex`, or a FIFO,
    // passed unseen. Nor is a name alone build output: a regular file or
    // symlink named `target` was passed over like the directory, whatever
    // it held. A `target` that is (or leads to) a directory still is, and
    // anything named `.git` (a repository's directory, or the file of a
    // worktree or submodule) is git's, not the tree's.
    let build = exe.and_then(|exe| {
        let top = build_dir_top(root, exe)?;
        Some(root.join(top.strip_prefix(root.canonicalize().ok()?).ok()?))
    });
    Ok(walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let skipped = match e.file_name().to_str() {
                Some(".git") => true,
                Some("target") => e.path().is_dir(),
                _ => false,
            };
            !(skipped && e.depth() > 0) && Some(e.path()) != build.as_deref()
        })
        .filter_map(Result::ok)
        // The top itself is the tree, even reached through a symlink, which
        // the walk follows but still reports as one.
        .filter(|e| e.depth() > 0 && !e.file_type().is_dir())
        .filter_map(|e| {
            let relative = e.path().strip_prefix(root).ok()?;
            Some(File {
                name: relative.to_string_lossy().replace('\\', "/"),
                utf8_name: relative.to_str().is_some(),
                source: Source::Worktree {
                    path: e.path().to_owned(),
                    index: Vec::new(),
                },
            })
        })
        .collect())
}

/// The Cargo target directory `exe` was built in, as a `/`-separated path
/// relative to `root`, when it lies inside `root`.
///
/// Cargo builds into `CARGO_TARGET_DIR` (or `build.target-dir`) wherever it
/// points, and marks the top of it with a `CACHEDIR.TAG`. Set inside the
/// checkout under another name than the `target` that `.gitignore` covers,
/// for example to keep a worktree's build beside it, every object file
/// there was listed as untracked: `cargo xtask policy` and the
/// `repository_passes_policy` test failed on hundreds of build products,
/// and the tests found no unseen place for their temporary repositories.
///
/// Only the directory this program itself runs from is passed over, and
/// only its untracked files: what is tracked or staged there is still read,
/// and the staged check, which decides what a commit holds, lists every
/// staged path as before. The checkout itself is never passed over.
///
/// The path is kept as the bytes git prints, not as text: a checkout or
/// target directory whose name is not UTF-8 (a home directory named in a
/// legacy encoding, say) was not recognised, and every object file under
/// it was listed and failed the full check.
fn build_dir_in(root: &Path, exe: &Path) -> Option<Vec<u8>> {
    let top = build_dir_top(root, exe)?;
    let root = root.canonicalize().ok()?;
    let relative = top.strip_prefix(&root).ok()?.as_os_str().as_encoded_bytes();
    // Git for Windows prints `/`; a Unix name may hold a `\\` of its own.
    Some(if cfg!(windows) {
        relative
            .iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect()
    } else {
        relative.to_vec()
    })
}

/// The canonical path of the Cargo target directory `exe` was built in,
/// when it lies inside `root` (see `build_dir_in`).
fn build_dir_top(root: &Path, exe: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let exe = exe.canonicalize().ok()?;
    exe.ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(&root) && *dir != root)
        .filter(|dir| {
            std::fs::read(dir.join("CACHEDIR.TAG"))
                .is_ok_and(|tag| tag.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55"))
        })
        .last()
        .map(Path::to_path_buf)
}

/// Whether `path` is a symlink to the Cargo target directory `exe` was
/// built in, wherever that is.
///
/// Git lists a symlink to a directory as one untracked file and does not
/// look inside, and the `/target/` rule in `.gitignore` matches only a
/// directory. A `target` kept on another disk or a tmpfs through a symlink,
/// or a `CARGO_TARGET_DIR` reached through one, was therefore listed as an
/// untracked file named `target` and failed the full check (`build_dir_in`
/// sees only the canonical path, which lies outside the checkout). Such a
/// link is passed over only while it leads to a directory holding Cargo's
/// `CACHEDIR.TAG` that the running program was built in; any other link is
/// still read, and a tracked or staged one always is.
fn leads_to_build_dir(path: &Path, exe: &Path) -> bool {
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return false;
    }
    let (Ok(dir), Ok(exe)) = (path.canonicalize(), exe.canonicalize()) else {
        return false;
    };
    dir != exe
        && exe.starts_with(&dir)
        && std::fs::read(dir.join("CACHEDIR.TAG"))
            .is_ok_and(|tag| tag.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55"))
}

/// Whether the `/`-separated path `raw` lies under the directory `dir`.
fn is_under(raw: &[u8], dir: Option<&[u8]>) -> bool {
    dir.is_some_and(|dir| {
        raw.strip_prefix(dir)
            .is_some_and(|rest| rest.first() == Some(&b'/'))
    })
}

/// What the index holds that the next commit adds or changes.
fn staged_files(root: &Path) -> Result<Vec<File>> {
    // --raw gives each entry's new mode and object id, so content is read by
    // id and submodules are seen as such. T: a path whose type changed (a
    // symlink replaced by a regular file, or the reverse) carries new content
    // just as a modification does. --no-renames: a rename is checked as the
    // addition it is, with one path per record. --ignore-submodules=none:
    // `diff.ignoreSubmodules` or an `ignore = all` line in .gitmodules (read
    // from the working tree, even untracked) would otherwise leave a staged
    // submodule out of the list.
    let fields = git_z(
        root,
        &[
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            "--diff-filter=ACMT",
            "--ignore-submodules=none",
        ],
    )?
    .context("--staged needs a git repository")?;
    let mut files = Vec::new();
    let mut fields = fields.into_iter();
    while let Some(header) = fields.next() {
        // :<old mode> <new mode> <old id> <new id> <status>, then the path.
        let header = String::from_utf8_lossy(&header);
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let [_, mode, _, oid, _] = parts[..] else {
            bail!("unexpected `git diff --raw` record: {header:?}");
        };
        let raw = fields
            .next()
            .with_context(|| format!("`git diff --raw` record without a path: {header:?}"))?;
        let (name, utf8_name, _) = decode(root, &raw);
        files.push(File {
            name,
            utf8_name,
            source: Source::Index {
                mode: mode.to_owned(),
                oid: oid.to_owned(),
            },
        });
    }
    Ok(files)
}

/// `None` if `path` has no exception; otherwise whether `content` is the
/// content it was granted for. The exception covers that content only, so
/// nothing else (an asset, an executable, a file of any size) can be
/// committed under the exempted name.
fn exempt(path: &str, content: &[u8]) -> Option<Result<(), String>> {
    let (_, pinned, _) = EXCEPTIONS.iter().find(|(p, ..)| *p == path)?;
    let actual = blake3::hash(content).to_hex();
    Some(if actual.as_str() == *pinned {
        Ok(())
    } else {
        Err(format!(
            "exempted only with BLAKE3 {pinned}, but its content hashes to {actual}; \
             a new version needs the owner's approval and its hash in xtask/src/policy.rs"
        ))
    })
}

/// All problems with one file. Pure, for testing.
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    // Only these synthetic Metal test sources may use Python or Swift.
    // All size, magic, NUL and encoded-data checks still apply.
    let allowed = matches!(
        path,
        "tests/metal/generate.py" | "tests/metal/run.py" | "tests/metal/probe.swift"
    ) || ALLOWED_NAMES.contains(&name)
        || ext
            .as_deref()
            .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e));
    if !allowed {
        problems.push("file type not on the allowlist".to_owned());
    }
    if content.len() > MAX_FILE_SIZE {
        problems.push(format!(
            "{} bytes exceeds the {MAX_FILE_SIZE}-byte limit",
            content.len()
        ));
    }
    for (magic, what) in FORBIDDEN_MAGIC {
        if content.starts_with(magic) {
            problems.push(format!("looks like a {what}"));
        }
    }
    if LFS_POINTER_PREFIXES
        .iter()
        .any(|prefix| content.starts_with(prefix))
    {
        problems.push(
            "a Git LFS pointer: the content it stands for is stored outside the repository, \
             unchecked"
                .to_owned(),
        );
    }
    if content.contains(&0) {
        problems.push("binary content (NUL bytes)".to_owned());
    } else {
        // Binary data need not hold a NUL: byte stuffing such as COBS
        // removes every one, and what is left passed as text, since bytes
        // over 0x7F break every run below. Text is UTF-8 here, without
        // control characters other than tab, line breaks and form feed.
        match std::str::from_utf8(content) {
            Err(e) => problems.push(format!(
                "not UTF-8 text (binary content?): invalid byte at offset {}",
                e.valid_up_to()
            )),
            Ok(text) => {
                if let Some((at, c)) = text
                    .char_indices()
                    .find(|&(_, c)| c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\x0c'))
                {
                    problems.push(format!(
                        "control character U+{:04X} at offset {at} (binary content?)",
                        c as u32
                    ));
                }
            }
        }
        let run = longest_encoded_run(content);
        if run > MAX_ENCODED_RUN {
            problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
        } else {
            let run = longest_printable_run(content);
            if run > MAX_ENCODED_RUN {
                problems.push(format!(
                    "{run}-character run without whitespace (ascii85, Z85, uuencoded or base65536 data?)"
                ));
            }
        }
        let run = longest_literal_run(content);
        if run > MAX_LITERAL_RUN {
            problems.push(format!(
                "{run}-element numeric array or byte-escape string (embedded data?)"
            ));
        }
        let run = longest_hex_dump(content);
        if run > MAX_LITERAL_RUN {
            problems.push(format!(
                "{run}-value hex dump (xxd, hexdump or od output: embedded data?)"
            ));
        }
    }
    problems
}

/// Longest run of characters that could be hex or base64 payload.
///
/// A single line break (LF or CRLF) between two payload characters does not
/// end a run: `base64` wraps its output at 76 columns, PEM and
/// `openssl base64` at 64, `xxd -p` at 60, so data encoded by any of them
/// would otherwise pass as many short runs. The break itself is not counted.
///
/// Both base64 alphabets count, each on its own: the standard one (`+`, `/`)
/// and the URL- and filename-safe one (`-`, `_`; RFC 4648 section 5) that
/// JWTs, `basenc --base64url` and most web tooling write. Measured with the
/// standard alphabet alone, base64url breaks at every `-` and `_`, one
/// character in 32 on average, and a file of it passes as short runs.
fn longest_encoded_run(content: &[u8]) -> usize {
    longest_run(content, b"+/=").max(longest_run(content, b"-_="))
}

/// Longest run of alphanumerics and `extra` bytes, joined across single line
/// breaks as `longest_encoded_run` describes.
fn longest_run(content: &[u8], extra: &[u8]) -> usize {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || extra.contains(b);
    let (mut longest, mut run, mut i) = (0, 0, 0);
    while let Some(b) = content.get(i) {
        if is_payload(b) {
            run += 1;
            longest = longest.max(run);
            i += 1;
            continue;
        }
        let line_break = match &content[i..] {
            [b'\r', b'\n', ..] => 2,
            [b'\n', ..] => 1,
            _ => 0,
        };
        if line_break > 0 && run > 0 && content.get(i + line_break).is_some_and(is_payload) {
            i += line_break;
            continue;
        }
        run = 0;
        i += 1;
    }
    longest
}

/// Shortest line after which a line break continues a printable run.
const MIN_WRAPPED_LINE: usize = 40;

/// Longest run of characters that are neither whitespace nor control
/// characters, counted in characters.
///
/// ascii85 (`!` to `u`: Python's `a85encode`, PostScript, PDF), Z85
/// (`basenc --z85`, ZeroMQ) and uuencode (`` ` `` to `_`) put punctuation in
/// almost every few characters, so their output never makes a long hex or
/// base64 run and a file of it passed. Text has whitespace far more often:
/// the longest such run in the tree is a few dozen characters.
///
/// Characters beyond ASCII count as well: base32768 and base65536 carry 15
/// or 16 bits in each CJK or other letter they write, with no whitespace,
/// and passed as text that no ASCII run saw at all.
///
/// A single line break continues the run as for base64, but only after a
/// line of at least `MIN_WRAPPED_LINE` characters: encoders wrap at 60 to 80
/// columns (`basenc` 76, `uuencode` 61), while a list of short entries
/// without spaces, such as `.gitignore` patterns, is not one long run.
fn longest_printable_run(content: &[u8]) -> usize {
    let text = String::from_utf8_lossy(content);
    let content: Vec<char> = text.chars().collect();
    let is_payload = |c: &char| !c.is_whitespace() && !c.is_control();
    let (mut longest, mut run, mut line, mut i) = (0, 0, 0, 0);
    while let Some(c) = content.get(i) {
        if is_payload(c) {
            run += 1;
            line += 1;
            longest = longest.max(run);
            i += 1;
            continue;
        }
        let line_break = match &content[i..] {
            ['\r', '\n', ..] => 2,
            ['\n', ..] => 1,
            _ => 0,
        };
        if line_break > 0
            && line >= MIN_WRAPPED_LINE
            && content.get(i + line_break).is_some_and(is_payload)
        {
            line = 0;
            i += line_break;
            continue;
        }
        (run, line) = (0, 0);
        i += 1;
    }
    longest
}

/// Longest list of numeric literals separated by commas, or run of byte
/// escapes (`\xHH`, `\u{..}`, `\uHHHH`, octal: see `escape_len`), across
/// string boundaries as `string_join_len` describes, in `content`.
///
/// Neither is a hex/base64 run: `xxd -i` and `bin2c` write a file as
/// `0x4d, 0x5a, 0x90, ...`, and a byte string as `"\x4d\x5a\x90..."`,
/// each token two to four characters long. So an executable pasted into a
/// `const DATA: [u8; N]` passed as long as the file stayed under the size
/// limit. Whitespace and line breaks around the commas do not end a list;
/// anything else (a bracket, a comment, an identifier) does, so the rows of
/// a two-dimensional table count one by one.
fn longest_literal_run(content: &[u8]) -> usize {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let (mut longest, mut i) = (0, 0);
    // Comma-separated literals: `0x4d`, `77u8`, `1_000`, `0.5`, `-3`.
    let mut run = 0;
    while i < content.len() {
        let starts_literal = match content[i] {
            b'0'..=b'9' => true,
            b'-' => content.get(i + 1).is_some_and(u8::is_ascii_digit),
            _ => false,
        } && (i == 0 || !ident(content[i - 1]));
        if !starts_literal {
            run = 0;
            i += 1;
            continue;
        }
        i += 1;
        while content.get(i).is_some_and(|&b| ident(b) || b == b'.') {
            i += 1;
        }
        run += 1;
        longest = longest.max(run);
        let mut j = i;
        while content.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        if content.get(j) == Some(&b',') {
            j += 1;
            while content.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            i = j;
        } else {
            run = 0;
        }
    }
    // Escapes back to back, joined across string boundaries.
    let (mut run, mut i) = (0, 0);
    while i < content.len() {
        if let Some(len) = escape_len(&content[i..]) {
            run += 1;
            longest = longest.max(run);
            i += len;
            let join = string_join_len(&content[i..]);
            if join > 0 && escape_len(&content[i + join..]).is_some() {
                i += join;
            }
            continue;
        }
        run = 0;
        i += 1;
    }
    longest
}

/// Length of the byte escape at the start of `rest`: `\xHH` (Rust, C,
/// HLSL), `\u{H}` to `\u{HHHHHH}` (Rust), which `escape_default` and
/// `{:?}` write for any character outside printable ASCII, `\uHHHH` and
/// `\UHHHHHHHH` (JSON, JavaScript, Java, C, Python: `json.dumps` writes
/// every byte over 0x7F so), or `\N` to `\NNN` in octal (C, HLSL, Python,
/// shell `printf`, and `od -b` or `repr` output pasted into a string).
fn escape_len(rest: &[u8]) -> Option<usize> {
    let hex_digits = |digits: &[u8], n: usize| {
        digits.len() >= n && digits[..n].iter().all(u8::is_ascii_hexdigit)
    };
    match rest {
        [b'\\', b'x', h, l, ..] if h.is_ascii_hexdigit() && l.is_ascii_hexdigit() => Some(4),
        [b'\\', b'u', b'{', digits @ ..] => {
            let n = digits.iter().take_while(|b| b.is_ascii_hexdigit()).count();
            ((1..=6).contains(&n) && digits.get(n) == Some(&b'}')).then_some(n + 4)
        }
        [b'\\', b'u', digits @ ..] if hex_digits(digits, 4) => Some(6),
        [b'\\', b'U', digits @ ..] if hex_digits(digits, 8) => Some(10),
        [b'\\', digits @ ..] => {
            let n = digits
                .iter()
                .take(3)
                .take_while(|b| (b'0'..=b'7').contains(*b))
                .count();
            (n > 0).then_some(n + 1)
        }
        _ => None,
    }
}

/// Length of what closes one string of escapes and opens the next, so that
/// the strings count as one run: a Rust string continuation (`\` and a line
/// break, then indentation), adjacent literals (`"\x4d" "\x5a"`, which C and
/// HLSL concatenate), or literals separated by commas (`concat!("\x4d",
/// "\x5a")`, `[b"\x4d", b"\x5a"]`). Without it, a string of escapes wrapped
/// every hundred escapes passed whatever its length. Zero if `rest` starts
/// with anything else, or with whitespace alone: inside one string that is
/// text, as in `"\x1b[0m \x1b[1m"`.
fn string_join_len(rest: &[u8]) -> usize {
    let (mut i, mut boundary) = (0, false);
    loop {
        let len = match &rest[i..] {
            [b'\\', b'\r', b'\n', ..] => 3,
            [b'\\', b'\n', ..] => 2,
            [b'b', b'"', ..] => 2,
            [b'"', ..] => 1,
            [b, ..] if b.is_ascii_whitespace() || *b == b',' => {
                i += 1;
                continue;
            }
            _ => return if boundary { i } else { 0 },
        };
        (i, boundary) = (i + len, true);
    }
}

/// Fewest hex values on a line for it to count as a line of a dump.
const MIN_DUMP_LINE_VALUES: usize = 4;

/// Number of hex values in the longest block of consecutive lines that read
/// as a hex dump.
///
/// `xxd` (`00000000: 4d5a 9000 ...`), `hexdump -C` (`00000000  4d 5a 90 00
/// ... |MZ..|`) and `od` (`0000000 055115 000220 ...`) write a file as short
/// hex or octal groups separated by spaces, with an offset in front and
/// sometimes the printable bytes behind. No group is a long hex run, there is
/// whitespace every few characters, and no comma joins them into an array,
/// so a dump of an executable pasted into a `.txt` or `.md` passed every
/// other check while it stayed under the size limit.
///
/// A line counts when at least `MIN_DUMP_LINE_VALUES` of its
/// whitespace-separated words, and three in four of them, are hex values (up
/// to 16 hex digits, after an optional `0x` and before an optional `:`), so
/// the offset and the printable column do not stop a dump while prose,
/// even prose fond of `cafe`, `dead` and `beef`, does not count. The values of
/// consecutive counting lines add up; any other line ends the block.
fn longest_hex_dump(content: &[u8]) -> usize {
    let is_value = |word: &[u8]| {
        let word = word.strip_suffix(b":").unwrap_or(word);
        let word = word
            .strip_prefix(b"0x")
            .or_else(|| word.strip_prefix(b"0X"))
            .unwrap_or(word);
        (1..=16).contains(&word.len()) && word.iter().all(u8::is_ascii_hexdigit)
    };
    let (mut longest, mut run) = (0, 0);
    for line in content.split(|&b| b == b'\n') {
        let words = line
            .split(u8::is_ascii_whitespace)
            .filter(|w| !w.is_empty());
        let (mut values, mut total) = (0, 0);
        for word in words {
            total += 1;
            values += usize::from(is_value(word));
        }
        if values >= MIN_DUMP_LINE_VALUES && values * 4 >= total * 3 {
            run += values;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::{MAX_ENCODED_RUN, MAX_LITERAL_RUN, check};

    /// Enforces the policy wherever tests run, even in clones without the hook.
    #[test]
    fn repository_passes_policy() {
        super::run(&crate::workspace_root(), false).unwrap();
    }

    #[test]
    fn accepts_source() {
        assert!(check("crates/x/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(check(".gitignore", b"/target/\n").is_empty());
        assert!(check(".githooks/pre-commit", b"#!/bin/sh\n").is_empty());
    }

    #[test]
    fn metal_test_sources_are_still_checked() {
        assert!(check("tests/metal/generate.py", b"import struct").is_empty());
        assert!(check("tests/metal/run.py", b"import subprocess").is_empty());
        assert!(check("tests/metal/probe.swift", b"import Metal").is_empty());

        assert!(!check("other.py", b"pass").is_empty());
        assert!(!check("other.swift", b"import Metal").is_empty());
        assert!(!check("tests/metal/probe.swift", b"MZ\x00").is_empty());
        assert!(!check("tests/metal/run.py", &vec![b'a'; 800]).is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!check("assets/tex.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(
            !check("notes.txt", b"MZ\x90\0").is_empty(),
            "magic wins over extension"
        );
        assert!(
            !check("data.bin", b"hello").is_empty(),
            "extension not allowed"
        );
        assert!(
            !check("a.rs", &vec![b'a'; 600 * 1024]).is_empty(),
            "too big"
        );
    }

    /// Runs git in `root` with a throwaway identity, asserting success.
    ///
    /// The identity and dates are set as variables, not `user.*` config:
    /// the `GIT_AUTHOR_*` and `GIT_COMMITTER_*` an environment sets
    /// (`git commit --date` and `git rebase` export them to hooks) win over
    /// config, and an empty name or a date git cannot parse fails the commit.
    #[cfg(unix)]
    fn git(root: &std::path::Path, args: &[&std::ffi::OsStr]) -> Vec<u8> {
        let out = super::git_command(root)
            .envs(IDENTITY)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        out.stdout
    }

    /// A fixed author and committer, so commits in temporary repositories
    /// neither take the environment's nor fail on it.
    #[cfg(unix)]
    const IDENTITY: [(&str, &str); 6] = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@t"),
        ("GIT_AUTHOR_DATE", "@0 +0000"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@t"),
        ("GIT_COMMITTER_DATE", "@0 +0000"),
    ];

    #[cfg(unix)]
    fn git_str(root: &std::path::Path, args: &[&str]) -> Vec<u8> {
        let args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        git(root, &args)
    }

    /// A temporary directory the checkout's own full check cannot see.
    ///
    /// Under a TMPDIR inside the checkout, a test's temporary repository is
    /// an untracked directory there, and `repository_passes_policy`, running
    /// at the same time, fails on it and on the files the test stages. The
    /// directory is then made beside the test binary instead, which is
    /// ignored as build output; if that is not ignored either, the test says
    /// so rather than flaking.
    ///
    /// Nor is it made under a path holding `:`: git splits
    /// `GIT_CEILING_DIRECTORIES` there, with no way to escape it, so the
    /// ceiling `git_command` sets would not hold and git would look for a
    /// repository above the temporary one.
    ///
    /// Nor under a path that is not UTF-8: the hook and filter scripts the
    /// tests write, and the arguments some of them pass to git, are built
    /// from the path as text, so every such test panicked in `to_str`.
    fn tempdir() -> tempfile::TempDir {
        let candidates = [
            Some(std::env::temp_dir()),
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf)),
        ];
        tempdir_in_first(&crate::workspace_root(), candidates.into_iter().flatten())
    }

    fn tempdir_in_first(
        checkout: &std::path::Path,
        candidates: impl IntoIterator<Item = std::path::PathBuf>,
    ) -> tempfile::TempDir {
        let checkout = checkout.canonicalize().unwrap();
        for dir in candidates {
            let Ok(dir) = dir.canonicalize() else {
                continue;
            };
            if dir.to_str().is_none()
                || cfg!(unix) && dir.as_os_str().as_encoded_bytes().contains(&b':')
            {
                continue;
            }
            if let Ok(tmp) = tempfile::tempdir_in(dir)
                && unseen_by_full_check(&checkout, tmp.path())
            {
                return tmp;
            }
        }
        panic!(
            "could not create a temporary directory outside {} or ignored by it: \
             set TMPDIR to a writable directory outside the checkout, named in UTF-8 \
             without `:`",
            checkout.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn temporary_directories_require_a_writable_and_unseen_location() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        let ignore = root.join(".gitignore");
        std::fs::write(&ignore, "/visible/probe\n/ignored/\n").unwrap();
        let visible = root.join("visible");
        let ignored = root.join("ignored");
        std::fs::create_dir(&visible).unwrap();
        std::fs::create_dir(&ignored).unwrap();

        // A regular file cannot host a fixture. An ignore rule for a single
        // filename also leaves its siblings visible to the checkout scan.
        let tmp = tempdir_in_first(root, [ignore, visible.clone(), ignored.clone()]);
        assert_eq!(tmp.path().parent(), Some(ignored.as_path()));
        assert_eq!(std::fs::read_dir(visible).unwrap().count(), 0);
        std::fs::write(tmp.path().join("fixture.rs"), b"fixture\n").unwrap();
        assert_eq!(names(root, false), [".gitignore"]);
    }

    /// A TMPDIR inside the checkout's `.git` directory is used as it is:
    /// nothing under it is ever listed, though no ignore rule covers it.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_live_in_the_git_directory() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        let visible = root.join("visible");
        let scratch = root.join(".git/scratch");
        std::fs::create_dir(&visible).unwrap();
        std::fs::create_dir(&scratch).unwrap();

        let tmp = tempdir_in_first(root, [visible.clone(), scratch.clone()]);
        assert_eq!(tmp.path().parent(), Some(scratch.as_path()));
        assert_eq!(std::fs::read_dir(visible).unwrap().count(), 0);
        std::fs::write(tmp.path().join("fixture.rs"), b"fixture\n").unwrap();
        assert!(names(root, false).is_empty());
        // Git itself does not call it ignored; only the shortcut lets it be.
        let status = super::git_command(root)
            .args(["check-ignore", "-q", "--", ".git/scratch"])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(1));
    }

    /// A TMPDIR is judged by where it leads and by every ignore source the
    /// full check reads: a symlink from outside into a visible directory of
    /// the checkout is refused, while a directory ignored only by
    /// `.git/info/exclude` or by a nested `.gitignore` is accepted, as
    /// `ls-files --exclude-standard` lists nothing under either.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_follow_symlinks_and_every_ignore_source() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        let visible = root.join("visible");
        let excluded = root.join("excluded");
        let nested = root.join("sub/scratch");
        for dir in [&visible, &excluded, &nested] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "/excluded/\n").unwrap();
        std::fs::write(root.join("sub/.gitignore"), "/scratch/\n").unwrap();
        let link = outside.path().join("link");
        std::os::unix::fs::symlink(&visible, &link).unwrap();

        let tmp = tempdir_in_first(root, [link.clone(), excluded.clone()]);
        assert_eq!(tmp.path().parent(), Some(excluded.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        let tmp_nested = tempdir_in_first(root, [link, nested.clone()]);
        assert_eq!(tmp_nested.path().parent(), Some(nested.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        for dir in [tmp.path(), tmp_nested.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        assert_eq!(names(root, false), ["sub/.gitignore"]);
        super::run(root, false).unwrap();
    }

    /// Outside any repository, the walk is what lists files, so a TMPDIR is
    /// unseen only under a `target` or `.git` directory, at any depth, as
    /// the walk skips them; an ignore file there means nothing.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_outside_a_repository_follow_the_walk() {
        let outer = tempdir();
        let root = outer.path();
        let visible = root.join("visible");
        let ignored = root.join("ignored");
        let nested = root.join("crates/x/target/tmp");
        let git_dir = root.join("sub/.git");
        for dir in [&visible, &ignored, &nested, &git_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "/ignored/\n").unwrap();
        let outside = super::git_command(root)
            .args(["rev-parse", "--git-dir"])
            .output()
            .unwrap();
        assert!(!outside.status.success(), "a repository above was found");

        let tmp = tempdir_in_first(root, [visible.clone(), ignored.clone(), nested.clone()]);
        assert_eq!(tmp.path().parent(), Some(nested.as_path()));
        let tmp_git = tempdir_in_first(root, [git_dir.clone()]);
        for dir in [&visible, &ignored] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(tmp_git.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(root, false), [".gitignore"]);
        super::run(root, false).unwrap();
    }

    #[cfg(unix)]
    const CACHEDIR_TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n\
        # This file is a cache directory tag created by cargo.\n";

    /// A Cargo target directory at `dir`, holding a stand-in for a program
    /// built there and one of its object files; returns the program's path.
    #[cfg(unix)]
    fn fake_build_dir(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir.join("debug/deps")).unwrap();
        std::fs::write(dir.join("CACHEDIR.TAG"), CACHEDIR_TAG).unwrap();
        std::fs::write(dir.join("debug/main.o"), b"\x7fELF\x02\x01\x01\0object").unwrap();
        let exe = dir.join("debug/deps/xtask-0123");
        std::fs::write(&exe, b"\x7fELF\x02\x01\x01\0program").unwrap();
        exe
    }

    #[cfg(unix)]
    fn names_built_by(root: &std::path::Path, exe: &std::path::Path) -> Vec<String> {
        super::list_files_built_by(root, false, Some(exe))
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect()
    }

    /// The build directory is its outermost `CACHEDIR.TAG` carrying Cargo's
    /// signature between the program and the checkout, never the checkout
    /// itself, and nothing at all for a program built elsewhere.
    #[cfg(unix)]
    #[test]
    fn build_directories_are_found_by_their_cache_tag() {
        let tmp = tempdir();
        let root = tmp.path();
        let exe = fake_build_dir(&root.join("out/build"));
        assert_eq!(
            super::build_dir_in(root, &exe).as_deref(),
            Some(&b"out/build"[..])
        );

        // An inner tag (cargo also tags some subdirectories) does not
        // narrow it; an outer one widens it, up to but not past the root.
        std::fs::write(root.join("out/build/debug/CACHEDIR.TAG"), CACHEDIR_TAG).unwrap();
        assert_eq!(
            super::build_dir_in(root, &exe).as_deref(),
            Some(&b"out/build"[..])
        );
        std::fs::write(root.join("out/CACHEDIR.TAG"), CACHEDIR_TAG).unwrap();
        assert_eq!(
            super::build_dir_in(root, &exe).as_deref(),
            Some(&b"out"[..])
        );
        std::fs::write(root.join("CACHEDIR.TAG"), CACHEDIR_TAG).unwrap();
        assert_eq!(
            super::build_dir_in(root, &exe).as_deref(),
            Some(&b"out"[..])
        );

        // A tag without Cargo's signature is not one.
        let other = tempdir();
        let exe = fake_build_dir(&other.path().join("b"));
        std::fs::write(other.path().join("b/CACHEDIR.TAG"), b"not a tag\n").unwrap();
        assert_eq!(super::build_dir_in(other.path(), &exe), None);

        // A program built outside the checkout passes nothing over.
        assert_eq!(
            super::build_dir_in(root, &fake_build_dir(&other.path().join("c"))),
            None
        );

        assert!(super::is_under(b"out/x", Some(b"out")));
        assert!(!super::is_under(b"output/x", Some(b"out")));
        assert!(!super::is_under(b"out", Some(b"out")));
        assert!(!super::is_under(b"out/x", None));
    }

    /// A CARGO_TARGET_DIR inside the checkout under another name than the
    /// ignored `target`, such as one kept beside a worktree, made the full
    /// check fail on every object file built there. Its untracked files are
    /// now passed over, in a repository and outside one; a file tracked
    /// there, or anything outside it, is still read, as is a build directory
    /// other than the one the check runs from.
    #[cfg(unix)]
    #[test]
    fn the_running_build_directory_is_passed_over() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&root.join("build-out"));
        let other = fake_build_dir(&root.join("build-other"));
        // Not passed over: the sibling whose name extends it.
        std::fs::create_dir(root.join("build-outer")).unwrap();
        std::fs::write(root.join("build-outer/x.rs"), b"fn x() {}\n").unwrap();
        let in_build = |names: &[String]| -> Vec<String> {
            names
                .iter()
                .filter(|n| n.starts_with("build-out/"))
                .cloned()
                .collect()
        };

        let listed = names_built_by(root, &exe);
        assert!(in_build(&listed).is_empty(), "{listed:?}");
        assert!(listed.contains(&"a.rs".to_owned()));
        assert!(listed.contains(&"build-outer/x.rs".to_owned()));
        assert!(listed.contains(&"build-other/debug/main.o".to_owned()));
        let listed = names_built_by(root, &other);
        assert!(listed.contains(&"build-out/debug/main.o".to_owned()));

        git_str(root, &["add", "-f", "build-out/debug/main.o"]);
        let listed = names_built_by(root, &exe);
        assert_eq!(in_build(&listed), ["build-out/debug/main.o"]);
        assert_eq!(names(root, true), ["build-out/debug/main.o"]);

        let outer = tempdir();
        let plain = outer.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&plain.join("build-out"));
        assert_eq!(names_built_by(&plain, &exe), ["a.rs"]);
    }

    /// A CARGO_TARGET_DIR inside the checkout whose name is not UTF-8 is
    /// passed over like any other: it was compared as text, never matched
    /// the bytes git prints, and each object file built there was listed.
    /// A sibling whose name merely starts with the same bytes is still read.
    #[cfg(unix)]
    #[test]
    fn a_build_directory_named_in_bytes_is_passed_over() {
        use std::os::unix::ffi::OsStrExt;
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        git_str(root, &["config", "core.quotePath", "true"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        let name = std::ffi::OsStr::from_bytes(b"build-\xff");
        if std::fs::create_dir(root.join(name)).is_err() {
            // Some filesystems (APFS) refuse names that are not UTF-8.
            return;
        }
        let exe = fake_build_dir(&root.join(name));
        let sibling = root.join(std::ffi::OsStr::from_bytes(b"build-\xff\xfe"));
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("x.rs"), b"fn x() {}\n").unwrap();

        assert_eq!(
            super::build_dir_in(root, &exe).as_deref(),
            Some(&b"build-\xff"[..])
        );
        let listed = names_built_by(root, &exe);
        assert_eq!(
            listed,
            ["a.rs", "build-\u{fffd}\u{fffd}/x.rs"],
            "{listed:?}"
        );

        // Outside a repository the walk passes it over too.
        let outer = tempdir();
        let plain = outer.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&plain.join(name));
        assert_eq!(names_built_by(&plain, &exe), ["a.rs"]);
    }

    /// A target directory kept elsewhere and reached through a symlink in
    /// the checkout, such as `target -> /mnt/fast/target`, was listed as an
    /// untracked file: git does not descend into a symlink, and `/target/`
    /// ignores only a directory. The link the check runs through is passed
    /// over; a link to any other build directory, or to a directory without
    /// Cargo's tag, is still read, as is the same link once it is tracked.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_build_directory_is_passed_over() {
        let repo = tempdir();
        let elsewhere = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/target/\n").unwrap();
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&elsewhere.path().join("build"));
        let other = fake_build_dir(&elsewhere.path().join("other"));
        let untagged = elsewhere.path().join("untagged");
        std::fs::create_dir(&untagged).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("build"), root.join("target")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("other"), root.join("other")).unwrap();
        std::os::unix::fs::symlink(&untagged, root.join("untagged")).unwrap();
        std::os::unix::fs::symlink(&exe, root.join("program")).unwrap();

        // Git really does see the link: the ignore rule does not cover it.
        let status = super::git_command(root)
            .args(["check-ignore", "-q", "--", "target"])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(1));

        assert_eq!(
            names_built_by(root, &exe),
            [".gitignore", "a.rs", "other", "program", "untagged"]
        );
        assert!(names_built_by(root, &other).contains(&"target".to_owned()));
        assert!(!super::leads_to_build_dir(
            &elsewhere.path().join("build"),
            &exe
        ));

        git_str(root, &["add", "-f", "target"]);
        assert!(names_built_by(root, &exe).contains(&"target".to_owned()));
        assert_eq!(names(root, true), ["target"]);
    }

    /// A build script may clone or `git init` a repository into its
    /// `OUT_DIR`, inside the build directory. Git lists such a repository as
    /// one untracked entry ending in `/`, which the full check refuses as a
    /// nested repository; under the running build directory it is build
    /// output like any other and is passed over, in a repository and outside
    /// one. The same repository anywhere else in the checkout, or in a build
    /// directory the check does not run from, is still refused.
    #[cfg(unix)]
    #[test]
    fn a_repository_inside_the_build_directory_is_passed_over() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&root.join("build-out"));
        let other = fake_build_dir(&root.join("build-other"));
        for dir in [
            "build-out/debug/build/x-0123/out/clone",
            "build-other/debug/build/x-0123/out/clone",
            "vendor/clone",
        ] {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            git_str(&dir, &["init", "-q"]);
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }

        let listed = names_built_by(root, &exe);
        assert!(
            !listed.iter().any(|n| n.starts_with("build-out/")),
            "{listed:?}"
        );
        assert!(listed.contains(&"vendor/clone/".to_owned()), "{listed:?}");
        assert!(
            listed.contains(&"build-other/debug/build/x-0123/out/clone/".to_owned()),
            "{listed:?}"
        );
        let listed = names_built_by(root, &other);
        assert!(
            listed.contains(&"build-out/debug/build/x-0123/out/clone/".to_owned()),
            "{listed:?}"
        );
        let files = super::list_files_built_by(root, false, Some(&exe)).unwrap();
        let vendor = files.iter().find(|f| f.name == "vendor/clone/").unwrap();
        let problem = super::read(root, vendor).unwrap_err();
        assert!(problem.contains("nested"), "{problem}");

        // Outside a repository the walk skips `.git` and the build directory
        // alike, so neither the clone's history nor its files are read.
        let outer = tempdir();
        let plain = outer.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&plain.join("build-out"));
        let clone = plain.join("build-out/debug/build/x-0123/out/clone");
        std::fs::create_dir_all(&clone).unwrap();
        git_str(&clone, &["init", "-q"]);
        std::fs::write(clone.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names_built_by(&plain, &exe), ["a.rs"]);
    }

    /// The checkout reached by another spelling (through a symlink, or a
    /// path through `..`) finds the same build directory: `build_dir_in`
    /// compares canonical paths but answers relative to the checkout, so
    /// git's listing and the walk, both made from the spelling given, pass
    /// it over alike. A program reached by another spelling is the same
    /// program, too.
    #[cfg(unix)]
    #[test]
    fn a_build_directory_is_found_however_the_checkout_is_reached() {
        let outer = tempdir();
        let links = tempdir();
        for repository in [true, false] {
            let name = if repository { "repo" } else { "plain" };
            let real = outer.path().join(name);
            std::fs::create_dir(&real).unwrap();
            if repository {
                git_str(&real, &["init", "-q"]);
            }
            std::fs::create_dir(real.join("src")).unwrap();
            std::fs::write(real.join("src/a.rs"), b"fn a() {}\n").unwrap();
            let exe = fake_build_dir(&real.join("build-out"));
            let link = links.path().join(name);
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let dotted = real.join("src").join("..");
            let exe_via_link = link.join("build-out/debug/deps/xtask-0123");

            for root in [&real, &link, &dotted] {
                for exe in [&exe, &exe_via_link] {
                    assert_eq!(
                        super::build_dir_in(root, exe).as_deref(),
                        Some(&b"build-out"[..]),
                        "{} {}",
                        root.display(),
                        exe.display()
                    );
                    assert_eq!(
                        names_built_by(root, exe),
                        ["src/a.rs"],
                        "{} {}",
                        root.display(),
                        exe.display()
                    );
                }
            }
        }
    }

    /// A CARGO_TARGET_DIR whose name merely contains a `.git` or `target`
    /// component spelled otherwise (`.git-build`, `target2`) is not skipped
    /// by those names: outside a repository the walk passes over only the
    /// running build directory, and a sibling build directory still has its
    /// object files read.
    #[cfg(unix)]
    #[test]
    fn build_directories_are_not_matched_by_name_alone() {
        let outer = tempdir();
        let plain = outer.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&plain.join(".git-build"));
        fake_build_dir(&plain.join("target2"));
        fake_build_dir(&plain.join("x/target"));

        let listed = names_built_by(&plain, &exe);
        assert!(
            !listed.iter().any(|n| n.starts_with(".git-build/")),
            "{listed:?}"
        );
        assert!(listed.contains(&"a.rs".to_owned()), "{listed:?}");
        assert!(
            listed.contains(&"target2/debug/main.o".to_owned()),
            "{listed:?}"
        );
        // A directory named `target`, at any depth, is skipped by the walk.
        assert!(!listed.iter().any(|n| n.starts_with("x/")), "{listed:?}");
    }

    /// A TMPDIR that is a directory but cannot be written to (a shared
    /// `/tmp` mounted read-only, or one made `0555`) is passed over for the
    /// next candidate, as a regular file is, and nothing is left in it.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_pass_over_a_read_only_candidate() {
        use std::os::unix::fs::PermissionsExt;
        let outer = tempdir();
        let locked = outer.path().join("locked");
        let plain = outer.path().join("plain");
        std::fs::create_dir(&locked).unwrap();
        std::fs::create_dir(&plain).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let restore = || {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        if std::fs::write(locked.join("probe"), b"").is_ok() {
            // Root, or a capability like it, writes through the mode bits.
            restore();
            return;
        }

        let tmp = tempdir_in_first(&crate::workspace_root(), [locked.clone(), plain.clone()]);
        assert!(tmp.path().starts_with(plain.canonicalize().unwrap()));
        assert_eq!(std::fs::read_dir(&locked).unwrap().count(), 0);
        restore();
    }

    /// A TMPDIR's name is matched against ignore rules as the literal path
    /// it is, never as a glob: under `/a/`, a visible directory named
    /// `[ab]` or `a*` (each a glob that would match `a`) is refused and
    /// left empty, while a directory named in non-ASCII UTF-8 (`scrätch`,
    /// `日本`), which `core.quotePath` would print quoted and escaped, is
    /// accepted when a rule ignores it, and listed unquoted when none does.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_by_their_literal_utf8_name() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        git_str(root, &["config", "core.quotePath", "true"]);
        std::fs::write(root.join(".gitignore"), "/a/\n/scrätch/\n/日本/\n").unwrap();
        let bracket = root.join("[ab]");
        let star = root.join("a*");
        let ignored = root.join("a");
        let accented = root.join("scrätch");
        let japanese = root.join("日本");
        let seen = root.join("vüe");
        for dir in [&bracket, &star, &ignored, &accented, &japanese, &seen] {
            std::fs::create_dir(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [bracket.clone(), star.clone(), accented.clone()]);
        assert_eq!(tmp.path().parent(), Some(accented.as_path()));
        let tmp_jp = tempdir_in_first(root, [seen.clone(), japanese.clone()]);
        assert_eq!(tmp_jp.path().parent(), Some(japanese.as_path()));
        for dir in [&bracket, &star, &ignored, &seen] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_jp.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(seen.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), [".gitignore", "vüe/a.rs"]);
        super::run(root, false).unwrap();
    }

    /// A TMPDIR that is unset to nothing, missing, or a dangling symlink is
    /// passed over for the next candidate, not panicked on, and nothing is
    /// made in its place.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_pass_over_missing_candidates() {
        let outer = tempdir();
        let missing = outer.path().join("missing");
        let dangling = outer.path().join("dangling");
        std::os::unix::fs::symlink(outer.path().join("gone"), &dangling).unwrap();
        let plain = outer.path().join("plain");
        std::fs::create_dir(&plain).unwrap();

        let tmp = tempdir_in_first(
            &crate::workspace_root(),
            [
                std::path::PathBuf::new(),
                missing.clone(),
                dangling.clone(),
                plain.clone(),
            ],
        );
        assert!(tmp.path().starts_with(plain.canonicalize().unwrap()));
        assert!(!missing.exists());
        assert!(!outer.path().join("gone").exists());
    }

    /// Git passes over any entry named `.git`, not only the repository's
    /// own: a TMPDIR kept under an empty `sub/.git` (left behind by a
    /// half-made clone, say) is unseen by the full check, as
    /// `unseen_by_full_check` assumes, and is not a nested repository.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_live_in_any_git_directory() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        let visible = root.join("sub/visible");
        let scratch = root.join("sub/.git/scratch");
        std::fs::create_dir_all(&visible).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(root.join("sub/a.rs"), b"fn a() {}\n").unwrap();

        let tmp = tempdir_in_first(root, [visible.clone(), scratch.clone()]);
        assert_eq!(tmp.path().parent(), Some(scratch.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(root, false), ["sub/a.rs"]);
        super::run(root, false).unwrap();
    }

    /// Only a component that is exactly `.git` is passed over: `.GIT` (git
    /// compares case only under `core.ignoreCase`, which a Unix checkout
    /// leaves off), `.gitx`, `x.git` and `.git ` are ordinary directories
    /// git lists, so a TMPDIR under any of them is refused and left empty,
    /// and the one under `sub/.git` is taken. What is written in the
    /// lookalikes afterwards is listed and checked.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_in_git_lookalikes_are_refused() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        git_str(root, &["config", "core.ignoreCase", "false"]);
        let lookalikes: Vec<_> = [".GIT", ".gitx", "x.git", ".git "]
            .iter()
            .map(|name| root.join("sub").join(name).join("scratch"))
            .collect();
        let scratch = root.join("sub/.git/scratch");
        for dir in lookalikes.iter().chain([&scratch]) {
            std::fs::create_dir_all(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, lookalikes.iter().cloned().chain([scratch.clone()]));
        assert_eq!(tmp.path().parent(), Some(scratch.as_path()));
        for dir in &lookalikes {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0, "{dir:?}");
        }
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert!(names(root, false).is_empty());
        super::run(root, false).unwrap();

        for dir in &lookalikes {
            std::fs::write(dir.join("a.rs"), b"fn a() {}\n").unwrap();
        }
        assert_eq!(
            names(root, false),
            [
                "sub/.GIT/scratch/a.rs",
                "sub/.git /scratch/a.rs",
                "sub/.gitx/scratch/a.rs",
                "sub/x.git/scratch/a.rs"
            ]
        );
        std::fs::write(lookalikes[0].join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert!(super::run(root, false).is_err());
    }

    /// A TMPDIR that is the checkout itself (`TMPDIR=.` run from it, or a
    /// link from elsewhere to it) puts each temporary directory at the top
    /// of the checkout, where nothing ignores it: it is refused, and the
    /// attempt left behind, so the next candidate is used and the checkout
    /// holds nothing new. Under a `/*` rule the same place is ignored and
    /// taken: the top of the checkout is judged like any other directory.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_at_the_top_of_the_checkout() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path().canonicalize().unwrap();
        git_str(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/ignored/\n").unwrap();
        let ignored = root.join("ignored");
        std::fs::create_dir(&ignored).unwrap();
        let link = outside.path().join("checkout");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let entries = |dir: &std::path::Path| -> Vec<std::ffi::OsString> {
            let mut names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let before = entries(&root);

        let tmp = tempdir_in_first(&root, [root.clone(), link, ignored.clone()]);
        assert_eq!(tmp.path().parent(), Some(ignored.as_path()));
        assert_eq!(entries(&root), before);
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(&root, false), [".gitignore"]);
        drop(tmp);

        std::fs::write(root.join(".gitignore"), "/*\n!/.gitignore\n").unwrap();
        let tmp = tempdir_in_first(&root, [root.clone()]);
        assert_eq!(tmp.path().parent(), Some(root.as_path()));
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(&root, false), [".gitignore"]);
        super::run(&root, false).unwrap();
    }

    /// A TMPDIR is judged by the directory made in it, not by the candidate:
    /// a rule naming only the temporary directories' own names (`.tmp*`,
    /// or `.tmp*/`, which matches directories alone) makes them ignored in
    /// an otherwise visible directory, and they are taken; a negation that
    /// re-includes those names under an ignored directory makes them seen,
    /// and they are refused and removed, though the directory holding them
    /// is ignored.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_the_directory_made() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "/reincluded/*\n!/reincluded/.tmp*\n/named/.tmp*\n/dirs/.tmp*/\n",
        )
        .unwrap();
        let reincluded = root.join("reincluded");
        let named = root.join("named");
        let dirs = root.join("dirs");
        for dir in [&reincluded, &named, &dirs] {
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("a.rs"), b"fn a() {}\n").unwrap();
        }
        let ignored = super::git_command(root)
            .args(["check-ignore", "-q", "--", "reincluded/a.rs"])
            .status()
            .unwrap();
        assert_eq!(ignored.code(), Some(0));

        let tmp = tempdir_in_first(root, [reincluded.clone(), named.clone()]);
        assert_eq!(tmp.path().parent(), Some(named.as_path()));
        let tmp_dirs = tempdir_in_first(root, [reincluded.clone(), dirs.clone()]);
        assert_eq!(tmp_dirs.path().parent(), Some(dirs.as_path()));
        assert_eq!(std::fs::read_dir(&reincluded).unwrap().count(), 1);
        for dir in [tmp.path(), tmp_dirs.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        assert_eq!(
            names(root, false),
            [".gitignore", "dirs/a.rs", "named/a.rs"]
        );
        super::run(root, false).unwrap();
    }

    /// A TMPDIR's name is matched as `.gitignore` syntax reads the rules,
    /// not as they are spelled: an unescaped `#` starts a comment and an
    /// unescaped trailing space is trimmed, so `#comment` and `space ` stay
    /// visible and are refused, as is a name holding a newline, which no
    /// rule line can spell; `\#hash`, `\!bang` and `trail\ ` name their
    /// directories exactly, and those are taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_as_gitignore_syntax_reads_them() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "#comment/\n/space \n/\\#hash/\n/\\!bang/\n/trail\\ /\n",
        )
        .unwrap();
        let comment = root.join("#comment");
        let spaced = root.join("space ");
        let newline = root.join("line\nbreak");
        let trimmed = root.join("space");
        let hash = root.join("#hash");
        let bang = root.join("!bang");
        let trail = root.join("trail ");
        for dir in [&comment, &spaced, &newline, &trimmed, &hash, &bang, &trail] {
            std::fs::create_dir(dir).unwrap();
        }
        // The trimmed rule ignores `space`, without its trailing space.
        let ignored = super::git_command(root)
            .args(["check-ignore", "-q", "--", "space"])
            .status()
            .unwrap();
        assert_eq!(ignored.code(), Some(0));

        let tmp = tempdir_in_first(root, [comment.clone(), spaced.clone(), hash.clone()]);
        assert_eq!(tmp.path().parent(), Some(hash.as_path()));
        let tmp_bang = tempdir_in_first(root, [newline.clone(), bang.clone()]);
        assert_eq!(tmp_bang.path().parent(), Some(bang.as_path()));
        let tmp_trail = tempdir_in_first(root, [trail.clone()]);
        assert_eq!(tmp_trail.path().parent(), Some(trail.as_path()));
        for dir in [&comment, &spaced, &newline] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_bang.path(), tmp_trail.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(spaced.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), [".gitignore", "space /a.rs"]);
        super::run(root, false).unwrap();
    }

    /// Git trims only unescaped trailing spaces from a rule and never
    /// normalizes the path it spells: `/tab\t` keeps its tab, so it ignores
    /// `tab\t` and leaves `tab` visible; `/two\ \ ` keeps both escaped
    /// spaces, so `two  ` is taken and `two` refused; and `./dot/` and
    /// `/dbl//` match nothing, so `dot` and `dbl` stay visible and are
    /// refused, left empty.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_by_rules_trimmed_only_of_spaces() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "/tab\t\n/two\\ \\ \n./dot/\n/dbl//\n",
        )
        .unwrap();
        let tab = root.join("tab");
        let tabbed = root.join("tab\t");
        let two = root.join("two");
        let spaced = root.join("two  ");
        let dot = root.join("dot");
        let dbl = root.join("dbl");
        for dir in [&tab, &tabbed, &two, &spaced, &dot, &dbl] {
            std::fs::create_dir(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [tab.clone(), dot.clone(), tabbed.clone()]);
        assert_eq!(tmp.path().parent(), Some(tabbed.as_path()));
        let tmp_two = tempdir_in_first(root, [two.clone(), dbl.clone(), spaced.clone()]);
        assert_eq!(tmp_two.path().parent(), Some(spaced.as_path()));
        for dir in [&tab, &two, &dot, &dbl] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_two.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(tab.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(dot.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(names(root, false), [".gitignore", "dot/b.rs", "tab/a.rs"]);
        super::run(root, false).unwrap();
    }

    /// A `core.excludesFile` set in the checkout's own `.git/config`, and
    /// kept outside the checkout, is an ignore source the full check reads
    /// as well: a TMPDIR ignored only there is taken, while a directory it
    /// does not name is refused and left empty.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_by_the_checkouts_excludes_file() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        let excludes = outside.path().join("excludes");
        std::fs::write(&excludes, "/via-config/\n").unwrap();
        git_str(
            root,
            &["config", "core.excludesFile", excludes.to_str().unwrap()],
        );
        let visible = root.join("visible");
        let via_config = root.join("via-config");
        std::fs::create_dir(&visible).unwrap();
        std::fs::create_dir(&via_config).unwrap();

        let tmp = tempdir_in_first(root, [visible.clone(), via_config.clone()]);
        assert_eq!(tmp.path().parent(), Some(via_config.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(visible.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), ["visible/a.rs"]);
        super::run(root, false).unwrap();
    }

    /// A `core.excludesFile` written into `.git/config` by hand names the
    /// file git's config parser reads out of it: unquoted, `#` and `;` open
    /// a comment and the spaces before it are trimmed, so `{out}/plain   #
    /// note` names `plain`, which takes `plain` and leaves `spaced`, named
    /// by the file `plain   `, visible and refused. Quoted, the same
    /// characters are kept, so `"{out}/kept # note"` names `kept # note`,
    /// which takes `quoted`, while `unquoted`, named only by `kept`, is
    /// refused, left empty.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_ignored_by_an_excludes_file_as_git_config_reads_it() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        let out = outside.path().to_str().unwrap();
        assert!(!out.contains(['"', '\\', '#', ';', ' ']));
        for (file, rule) in [
            ("plain", "/plain/\n"),
            ("plain   ", "/spaced/\n"),
            ("kept # note", "/quoted/\n"),
            ("kept", "/unquoted/\n"),
        ] {
            std::fs::write(outside.path().join(file), rule).unwrap();
        }
        let base = std::fs::read_to_string(root.join(".git/config")).unwrap();
        let set = |value: &str| {
            std::fs::write(
                root.join(".git/config"),
                format!("{base}[core]\n\texcludesFile = {value}\n"),
            )
            .unwrap();
        };
        let dir = |name: &str| root.join(name);
        for name in ["plain", "spaced", "quoted", "unquoted"] {
            std::fs::create_dir(dir(name)).unwrap();
        }
        let taken = |first: &[&str], last: &str| {
            let tmp = tempdir_in_first(root, first.iter().map(|name| dir(name)).chain([dir(last)]));
            assert_eq!(tmp.path().parent(), Some(dir(last).as_path()));
            for name in first {
                assert_eq!(std::fs::read_dir(dir(name)).unwrap().count(), 0);
            }
            std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
            tmp
        };

        set(&format!("{out}/plain   # note"));
        let tmp = taken(&["spaced", "quoted", "unquoted"], "plain");
        std::fs::write(dir("spaced").join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), ["spaced/a.rs"]);
        super::run(root, false).unwrap();
        drop(tmp);
        std::fs::remove_file(dir("spaced").join("a.rs")).unwrap();

        set(&format!("{out}/plain\t; note"));
        drop(taken(&["spaced", "quoted", "unquoted"], "plain"));

        set(&format!("\"{out}/kept # note\""));
        let tmp = taken(&["plain", "spaced", "unquoted"], "quoted");
        std::fs::write(dir("unquoted").join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(names(root, false), ["unquoted/b.rs"]);
        super::run(root, false).unwrap();
        drop(tmp);

        // Git agrees: it reads the quoted value whole.
        let value = super::git_command(root)
            .args(["config", "core.excludesFile"])
            .output()
            .unwrap();
        assert_eq!(value.stdout, format!("{out}/kept # note\n").as_bytes());
    }

    /// A relative `core.excludesFile` is read from the top of the checkout,
    /// wherever it is set and wherever git is run from: set in a file that
    /// `.git/config` includes (`include.path`, itself relative to `.git`),
    /// `rules` is the checkout's `rules`, not `.git/rules` beside the config
    /// that names it, nor a file under the directory git is run in. So
    /// `relative`, named in the checkout's `rules` (which also ignores
    /// itself), is taken, while `beside`, named only in `.git/rules`, and
    /// `sub`, named only in `sub/rules`, stay visible and are refused, left
    /// empty. Unset, the rule is gone and `relative` is refused too.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_by_a_relative_excludes_file() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("rules"), "/relative/\n/rules\n").unwrap();
        std::fs::write(root.join(".git/rules"), "/beside/\n").unwrap();
        std::fs::write(
            root.join(".git/extra.config"),
            "[core]\n\texcludesFile = rules\n",
        )
        .unwrap();
        git_str(root, &["config", "include.path", "extra.config"]);
        let relative = root.join("relative");
        let beside = root.join("beside");
        let sub = root.join("sub");
        for dir in [&relative, &beside, &sub] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(sub.join("rules"), "/sub/\n*\n").unwrap();
        let from_sub = super::git_command(root)
            .args([
                "-C",
                "sub",
                "check-ignore",
                "--",
                "../relative/a.rs",
                "a.rs",
            ])
            .output()
            .unwrap();
        assert_eq!(from_sub.status.code(), Some(0));
        assert_eq!(from_sub.stdout, b"../relative/a.rs\n");

        let tmp = tempdir_in_first(root, [beside.clone(), sub.clone(), relative.clone()]);
        assert_eq!(tmp.path().parent(), Some(relative.as_path()));
        assert_eq!(std::fs::read_dir(&beside).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&sub).unwrap().count(), 1);
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(beside.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(sub.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(names(root, false), ["beside/a.rs", "sub/b.rs", "sub/rules"]);
        drop(tmp);
        std::fs::remove_file(sub.join("rules")).unwrap();
        std::fs::remove_dir_all(&beside).unwrap();
        std::fs::remove_dir_all(&sub).unwrap();
        super::run(root, false).unwrap();

        // With the included file gone (no error: a missing include is
        // skipped), nothing names `rules`, and it and `relative` are seen.
        std::fs::remove_file(root.join(".git/extra.config")).unwrap();
        std::fs::write(relative.join("c.rs"), b"fn c() {}\n").unwrap();
        assert_eq!(names(root, false), ["relative/c.rs", "rules"]);
    }

    /// A `core.excludesFile` set by a conditional include holds only while
    /// its condition does, as the full check reads it: `gitdir:` naming this
    /// `.git` matches, and so does `gitdir/i:` naming it in upper case,
    /// while `gitdir:` naming another directory does not; `onbranch:` holds
    /// for the branch `HEAD` names, even unborn. Each include names its own
    /// excludes file for the one key, so only the last include that holds
    /// counts: `branch`, then, with the branch includes gone, `folded`, then
    /// `exact`, is taken, while the rest stay visible and are refused, left
    /// empty. Once `HEAD` names `other`, its rule is last and holds instead,
    /// and `branch` is seen again.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_by_a_conditionally_included_excludes_file() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        git_str(root, &["symbolic-ref", "HEAD", "refs/heads/scratch"]);
        let git_dir = format!("{}/.git", root.to_str().unwrap());
        let mut config = String::new();
        for (name, condition) in [
            ("exact", format!("gitdir:{git_dir}")),
            ("folded", format!("gitdir/i:{}", git_dir.to_uppercase())),
            ("elsewhere", format!("gitdir:{git_dir}-elsewhere/")),
            ("branch", "onbranch:scratch".to_owned()),
            ("other", "onbranch:other".to_owned()),
        ] {
            let rules = outside.path().join(format!("{name}.rules"));
            std::fs::write(&rules, format!("/{name}/\n")).unwrap();
            let include = outside.path().join(format!("{name}.config"));
            std::fs::write(
                &include,
                format!("[core]\n\texcludesFile = {}\n", rules.to_str().unwrap()),
            )
            .unwrap();
            config.push_str(&format!(
                "[includeIf \"{condition}\"]\n\tpath = {}\n",
                include.to_str().unwrap()
            ));
        }
        let mut local = std::fs::read_to_string(root.join(".git/config")).unwrap();
        local.push_str(&config);
        std::fs::write(root.join(".git/config"), local).unwrap();
        let dir = |name: &str| root.join(name);
        for name in ["exact", "folded", "elsewhere", "branch", "other"] {
            std::fs::create_dir(dir(name)).unwrap();
        }
        let taken = |first: &[&str], last: &str| {
            let tmp = tempdir_in_first(root, first.iter().map(|name| dir(name)).chain([dir(last)]));
            assert_eq!(tmp.path().parent(), Some(dir(last).as_path()));
            for name in first {
                assert_eq!(std::fs::read_dir(dir(name)).unwrap().count(), 0);
            }
            tmp
        };
        // Only `branch` is ignored: the later `onbranch:other` does not hold.
        let tmp = taken(&["elsewhere", "other", "exact", "folded"], "branch");
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(dir("exact").join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), ["exact/a.rs"]);
        std::fs::remove_file(dir("exact").join("a.rs")).unwrap();

        // With the branch includes gone, `folded`, the last to hold, wins.
        let only_gitdir: String = std::fs::read_to_string(root.join(".git/config"))
            .unwrap()
            .split("[includeIf")
            .filter(|section| !section.contains("onbranch:"))
            .collect::<Vec<_>>()
            .join("[includeIf");
        std::fs::write(root.join(".git/config"), &only_gitdir).unwrap();
        drop(tmp);
        let tmp = taken(&["elsewhere", "other", "branch", "exact"], "folded");
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        drop(tmp);

        // And with `folded`'s gone too, `exact`, the first, is all that holds.
        let only_exact: String = only_gitdir
            .split("[includeIf")
            .filter(|section| !section.contains("gitdir/i:"))
            .collect::<Vec<_>>()
            .join("[includeIf");
        std::fs::write(root.join(".git/config"), &only_exact).unwrap();
        let tmp = taken(&["elsewhere", "other", "branch", "folded"], "exact");
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        drop(tmp);

        // Restored, and on `other`, its rule is last and `branch` is seen.
        let mut restored = only_exact.split("[includeIf").next().unwrap().to_owned();
        restored.push_str(&config);
        std::fs::write(root.join(".git/config"), restored).unwrap();
        git_str(root, &["symbolic-ref", "HEAD", "refs/heads/other"]);
        let tmp = taken(&["elsewhere", "branch", "exact", "folded"], "other");
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(dir("branch").join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(names(root, false), ["branch/b.rs"]);
        super::run(root, false).unwrap();
    }

    /// A `core.excludesFile` included on `hasconfig:remote.*.url:` holds
    /// only while some remote's URL matches the pattern, as the full check
    /// reads it: with no remote, or one whose host merely starts the same,
    /// `remote` stays visible and is refused, left empty, and `local`
    /// (ignored by `.git/info/exclude`) is taken; once `origin` matches,
    /// `remote` is taken. A later empty `excludesFile` clears the included
    /// one rather than naming a file, and `remote` is refused again.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_by_an_excludes_file_included_on_a_remote() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "/local/\n").unwrap();
        let rules = outside.path().join("remote.rules");
        std::fs::write(&rules, "/remote/\n").unwrap();
        let include = outside.path().join("remote.config");
        std::fs::write(
            &include,
            format!("[core]\n\texcludesFile = {}\n", rules.to_str().unwrap()),
        )
        .unwrap();
        let mut config = std::fs::read_to_string(root.join(".git/config")).unwrap();
        config.push_str(&format!(
            "[includeIf \"hasconfig:remote.*.url:https://example.invalid/**\"]\n\tpath = {}\n",
            include.to_str().unwrap()
        ));
        std::fs::write(root.join(".git/config"), config).unwrap();
        let remote = root.join("remote");
        let local = root.join("local");
        std::fs::create_dir(&remote).unwrap();
        std::fs::create_dir(&local).unwrap();
        let refused = || {
            let tmp = tempdir_in_first(root, [remote.clone(), local.clone()]);
            assert_eq!(tmp.path().parent(), Some(local.as_path()));
            assert_eq!(std::fs::read_dir(&remote).unwrap().count(), 0);
        };

        refused();
        let url = "https://example.invalid.other/game";
        git_str(root, &["remote", "add", "origin", url]);
        refused();

        git_str(
            root,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/game",
            ],
        );
        let tmp = tempdir_in_first(root, [remote.clone()]);
        assert_eq!(tmp.path().parent(), Some(remote.as_path()));
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(local.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), Vec::<String>::new());
        super::run(root, false).unwrap();
        drop(tmp);

        let mut config = std::fs::read_to_string(root.join(".git/config")).unwrap();
        config.push_str("[core]\n\texcludesFile =\n");
        std::fs::write(root.join(".git/config"), config).unwrap();
        refused();
        std::fs::write(remote.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(names(root, false), ["remote/b.rs"]);
    }

    /// A TMPDIR is unseen when any directory above it is ignored, however
    /// deep: `/outer/` covers `outer/deep/er`, and a negation under it
    /// (`!/outer/deep/`) re-includes nothing, since git never looks inside
    /// an ignored directory. `**/nested/**` ignores what is in `sub/nested`,
    /// though not `sub/nested` itself, and is enough; `scratch/**`, with a
    /// slash before its end, is anchored to the top and leaves `sub/scratch`
    /// visible, so that is refused. Under
    /// `core.ignoreCase=true` the rule `/UPPER/` ignores `upper`, as the
    /// full check reads it; without it, `upper` is visible and refused.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_ignored_ancestors_and_case() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        git_str(root, &["config", "core.ignoreCase", "false"]);
        std::fs::write(
            root.join(".gitignore"),
            "/outer/\n!/outer/deep/\nscratch/**\n**/nested/**\n/UPPER/\n",
        )
        .unwrap();
        let deep = root.join("outer/deep/er");
        let scratch = root.join("sub/scratch");
        let nested = root.join("sub/nested");
        let upper = root.join("upper");
        let visible = root.join("visible");
        for dir in [&deep, &scratch, &nested, &upper, &visible] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [visible.clone(), upper.clone(), deep.clone()]);
        assert_eq!(tmp.path().parent(), Some(deep.as_path()));
        let tmp_nested = tempdir_in_first(root, [scratch.clone(), nested.clone()]);
        assert_eq!(tmp_nested.path().parent(), Some(nested.as_path()));
        for dir in [&visible, &upper, &scratch] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }

        git_str(root, &["config", "core.ignoreCase", "true"]);
        let tmp_upper = tempdir_in_first(root, [visible.clone(), upper.clone()]);
        assert_eq!(tmp_upper.path().parent(), Some(upper.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        for dir in [tmp.path(), tmp_nested.path(), tmp_upper.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(visible.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), [".gitignore", "visible/a.rs"]);
        super::run(root, false).unwrap();
    }

    /// Ignore sources are weighed as git weighs them, not merely added up:
    /// a `.gitignore` beats `.git/info/exclude`, so `!/reopened/` there
    /// makes a directory excluded only in `info/exclude` visible, and it is
    /// refused; a deeper `.gitignore` beats a shallower one, so
    /// `sub/.gitignore`'s `!/scratch/` re-includes `sub/scratch` though the
    /// top level ignores it, and that is refused too. The excluded
    /// directory nothing re-includes is taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_ignore_precedence() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "/reopened/\n/excluded/\n").unwrap();
        std::fs::write(root.join(".gitignore"), "!/reopened/\n/sub/scratch/\n").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/.gitignore"), "!/scratch/\n").unwrap();
        let reopened = root.join("reopened");
        let scratch = root.join("sub/scratch");
        let excluded = root.join("excluded");
        for dir in [&reopened, &scratch, &excluded] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [reopened.clone(), scratch.clone(), excluded.clone()]);
        assert_eq!(tmp.path().parent(), Some(excluded.as_path()));
        for dir in [&reopened, &scratch] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(reopened.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(scratch.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [
                ".gitignore",
                "reopened/a.rs",
                "sub/.gitignore",
                "sub/scratch/b.rs"
            ]
        );
        super::run(root, false).unwrap();
    }

    /// Within one ignore file the last matching rule wins, as git reads it:
    /// `!/scratch/` after `/scr*/` re-includes `scratch`, which is refused,
    /// while `scrap` stays ignored and is taken; `/late/` after `!/late/`
    /// ignores `late` again, and that is taken. `/box/*` ignores what is in
    /// `box` but not `box` itself, so, unlike a negation under an ignored
    /// directory, `!/box/keep/` does re-include `box/keep`: it is refused,
    /// and `box/other` taken. Leading whitespace is part of a pattern, not
    /// trimmed: ` lead/` ignores a directory named ` lead`, which is taken,
    /// and leaves `lead` visible, so that is refused.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_the_last_matching_rule() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "/scr*/\n!/scratch/\n!/late/\n/late/\n/box/*\n!/box/keep/\n lead/\n",
        )
        .unwrap();
        let scratch = root.join("scratch");
        let scrap = root.join("scrap");
        let late = root.join("late");
        let keep = root.join("box/keep");
        let other = root.join("box/other");
        let lead = root.join("lead");
        let spaced = root.join(" lead");
        for dir in [&scratch, &scrap, &late, &keep, &other, &lead, &spaced] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let taken: Vec<_> = [&scrap, &late, &other, &spaced]
            .into_iter()
            .map(|dir| {
                let tmp = tempdir_in_first(
                    root,
                    [scratch.clone(), keep.clone(), lead.clone(), dir.clone()],
                );
                assert_eq!(tmp.path().parent(), Some(dir.as_path()));
                tmp
            })
            .collect();
        for dir in [&scratch, &keep, &lead] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for tmp in &taken {
            std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(scratch.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(keep.join("b.rs"), b"fn b() {}\n").unwrap();
        std::fs::write(lead.join("c.rs"), b"fn c() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [".gitignore", "box/keep/b.rs", "lead/c.rs", "scratch/a.rs"]
        );
        super::run(root, false).unwrap();
    }

    /// A TMPDIR is judged by where it leads, from inside the checkout as
    /// well as from outside: a symlink in the checkout to a directory
    /// elsewhere is taken, since git lists the link and never what it
    /// leads to. A file tracked with `add -f` under an ignored directory
    /// does not make the directory visible: a TMPDIR there is still taken,
    /// and only the tracked file is listed.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_follow_links_out_and_ignore_tracked_siblings() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/ignored/\n").unwrap();
        let ignored = root.join("ignored");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::write(ignored.join("kept.rs"), b"fn kept() {}\n").unwrap();
        git_str(root, &["add", "-f", "--", "ignored/kept.rs"]);
        let link = root.join("out.txt");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let tmp = tempdir_in_first(root, [link.clone()]);
        assert!(
            tmp.path()
                .starts_with(outside.path().canonicalize().unwrap())
        );
        let tmp_ignored = tempdir_in_first(root, [ignored.clone()]);
        assert_eq!(tmp_ignored.path().parent(), Some(ignored.as_path()));
        for dir in [tmp.path(), tmp_ignored.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        assert_eq!(
            names(root, false),
            [".gitignore", "ignored/kept.rs", "out.txt"]
        );
    }

    /// Ignore files are read as git reads them, not as they are stored: a
    /// rule ending in CRLF ignores `crlf`, its carriage return dropped, so a
    /// directory named `crlf\r` stays visible and is refused. A
    /// `.gitignore` that is a symlink is not followed at all (git opens it
    /// without following links), so a directory only it names is visible
    /// and refused too, though the file it leads to would ignore it. A
    /// candidate may ignore its own contents: under `self/.gitignore`
    /// holding `*`, the temporary directory made there is taken, and that
    /// `.gitignore`, matching its own rule, is not listed either.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_ignore_files_as_git_reads_them() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/crlf/\r\n").unwrap();
        let rules = outside.path().join("rules");
        std::fs::write(&rules, "/linked/\n").unwrap();
        let crlf = root.join("crlf");
        let carriage = root.join("crlf\r");
        let linked = root.join("sl/linked");
        let own = root.join("self");
        for dir in [&crlf, &carriage, &linked, &own] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(&rules, root.join("sl/.gitignore")).unwrap();
        std::fs::write(own.join(".gitignore"), "*\n").unwrap();

        let tmp = tempdir_in_first(root, [carriage.clone(), linked.clone(), crlf.clone()]);
        assert_eq!(tmp.path().parent(), Some(crlf.as_path()));
        let tmp_own = tempdir_in_first(root, [linked.clone(), own.clone()]);
        assert_eq!(tmp_own.path().parent(), Some(own.as_path()));
        for dir in [&carriage, &linked] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_own.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(carriage.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(linked.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [
                ".gitignore",
                "crlf\r/a.rs",
                "sl/.gitignore",
                "sl/linked/b.rs"
            ]
        );
        super::run(root, false).unwrap();
    }

    /// A UTF-8 byte order mark is dropped only where it opens an ignore
    /// file, as git reads it: `.gitignore` starting with one still ignores
    /// `bom`, and `.git/info/exclude` starting with one still ignores
    /// `excl`, so both are taken, while a directory named with the mark,
    /// `\u{feff}bom`, stays visible and is refused. On any later line the
    /// mark is part of the rule: `\u{feff}late/` takes a directory named
    /// with it and leaves `late` visible, so that is refused, left empty.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_ignore_files_opened_by_a_byte_order_mark() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "\u{feff}/bom/\n\u{feff}late/\n").unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "\u{feff}/excl/\n").unwrap();
        let bom = root.join("bom");
        let marked = root.join("\u{feff}bom");
        let excl = root.join("excl");
        let late = root.join("late");
        let marked_late = root.join("\u{feff}late");
        for dir in [&bom, &marked, &excl, &late, &marked_late] {
            std::fs::create_dir(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [marked.clone(), bom.clone()]);
        assert_eq!(tmp.path().parent(), Some(bom.as_path()));
        let tmp_excl = tempdir_in_first(root, [late.clone(), excl.clone()]);
        assert_eq!(tmp_excl.path().parent(), Some(excl.as_path()));
        let tmp_late = tempdir_in_first(root, [late.clone(), marked_late.clone()]);
        assert_eq!(tmp_late.path().parent(), Some(marked_late.as_path()));
        for dir in [&marked, &late] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_excl.path(), tmp_late.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(marked.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(late.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [".gitignore", "late/b.rs", "\u{feff}bom/a.rs"]
        );
        super::run(root, false).unwrap();
    }

    /// A NUL byte ends a rule where it stands, as git reads each line as a
    /// C string: `/nul\0junk/` is the rule `/nul`, so `nul` is taken and
    /// `nuljunk` stays visible and is refused, and `/cut\0/kept/` is the
    /// rule `/cut`, leaving `kept` visible and refused too. A last line no
    /// newline ends is still a rule: `/tail/`, unterminated, takes `tail`.
    /// The `.gitignore` itself, holding a NUL, is refused as binary.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_rules_a_nul_byte_cuts_short() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/nul\0junk/\n/cut\0/kept/\n/tail/").unwrap();
        let nul = root.join("nul");
        let junk = root.join("nuljunk");
        let kept = root.join("kept");
        let tail = root.join("tail");
        for dir in [&nul, &junk, &kept, &tail] {
            std::fs::create_dir(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [junk.clone(), nul.clone()]);
        assert_eq!(tmp.path().parent(), Some(nul.as_path()));
        let tmp_tail = tempdir_in_first(root, [kept.clone(), tail.clone()]);
        assert_eq!(tmp_tail.path().parent(), Some(tail.as_path()));
        for dir in [&junk, &kept] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_tail.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(junk.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(kept.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [".gitignore", "kept/b.rs", "nuljunk/a.rs"]
        );
        // The fixtures stay unseen; only the `.gitignore` is refused, as a
        // NUL byte marks it binary whatever git makes of its rules.
        assert!(
            !super::check(
                ".gitignore",
                &std::fs::read(root.join(".gitignore")).unwrap()
            )
            .is_empty()
        );
        for path in ["kept/b.rs", "nuljunk/a.rs"] {
            assert!(super::check(path, &std::fs::read(root.join(path)).unwrap()).is_empty());
        }
        assert!(super::run(root, false).is_err());
    }

    /// In a linked worktree, where `.git` is a file, the repository's
    /// shared `info/exclude` still applies: a TMPDIR it alone ignores in
    /// the worktree is taken, a visible sibling is refused, and only the
    /// worktree's own files are listed, not the main checkout's.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_by_a_linked_worktrees_shared_exclude() {
        let repo = tempdir();
        let main = repo.path().join("main");
        let linked = repo.path().join("linked");
        std::fs::create_dir(&main).unwrap();
        git_str(&main, &["init", "-q"]);
        std::fs::write(main.join("a.rs"), b"fn a() {}\n").unwrap();
        git_str(&main, &["add", "--", "a.rs"]);
        git_str(&main, &["commit", "-q", "-m", "a"]);
        git_str(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
        std::fs::create_dir_all(main.join(".git/info")).unwrap();
        std::fs::write(main.join(".git/info/exclude"), "/shared/\n").unwrap();
        assert!(linked.join(".git").is_file());
        let visible = linked.join("visible");
        let shared = linked.join("shared");
        std::fs::create_dir(&visible).unwrap();
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(main.join("main-only.rs"), b"fn m() {}\n").unwrap();

        let tmp = tempdir_in_first(&linked, [visible.clone(), shared.clone()]);
        assert_eq!(tmp.path().parent(), Some(shared.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(visible.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(names(&linked, false), ["a.rs", "visible/b.rs"]);
        super::run(&linked, false).unwrap();
    }

    /// A `core.excludesFile` in a linked worktree's own `config.worktree`
    /// holds there only once `extensions.worktreeConfig` is set: before,
    /// git skips that file, `own` stays visible in the worktree and is
    /// refused, left empty, and `shared` (ignored by `info/exclude`) is
    /// taken. Once set, `own` is taken in the linked worktree, while the
    /// main checkout, which never reads that file, still refuses its `own`.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_by_a_worktrees_own_config() {
        let repo = tempdir();
        let outside = tempdir();
        let main = repo.path().join("main");
        let linked = repo.path().join("linked");
        std::fs::create_dir(&main).unwrap();
        git_str(&main, &["init", "-q"]);
        std::fs::write(main.join("a.rs"), b"fn a() {}\n").unwrap();
        git_str(&main, &["add", "--", "a.rs"]);
        git_str(&main, &["commit", "-q", "-m", "a"]);
        git_str(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
        std::fs::create_dir_all(main.join(".git/info")).unwrap();
        std::fs::write(main.join(".git/info/exclude"), "/shared/\n").unwrap();
        let rules = outside.path().join("own.rules");
        std::fs::write(&rules, "/own/\n").unwrap();
        let worktree_config = main.join(".git/worktrees/linked/config.worktree");
        std::fs::write(
            &worktree_config,
            format!("[core]\n\texcludesFile = {}\n", rules.to_str().unwrap()),
        )
        .unwrap();
        for top in [&main, &linked] {
            std::fs::create_dir(top.join("own")).unwrap();
            std::fs::create_dir(top.join("shared")).unwrap();
        }
        let taken = |top: &std::path::Path, first: &[&str], last: &str| {
            let candidates = first.iter().chain([&last]).map(|name| top.join(name));
            let tmp = tempdir_in_first(top, candidates);
            assert_eq!(tmp.path().parent(), Some(top.join(last).as_path()));
            for name in first {
                assert_eq!(std::fs::read_dir(top.join(name)).unwrap().count(), 0);
            }
            tmp
        };

        // Without the extension, `config.worktree` is not read.
        drop(taken(&linked, &["own"], "shared"));
        std::fs::write(linked.join("own/b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(names(&linked, false), ["a.rs", "own/b.rs"]);
        std::fs::remove_file(linked.join("own/b.rs")).unwrap();

        git_str(&main, &["config", "extensions.worktreeConfig", "true"]);
        let tmp = taken(&linked, &[], "own");
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(&linked, false), ["a.rs"]);
        super::run(&linked, false).unwrap();

        // The main checkout has no `config.worktree` of its own.
        drop(taken(&main, &["own"], "shared"));
        std::fs::write(main.join("own/c.rs"), b"fn c() {}\n").unwrap();
        assert_eq!(names(&main, false), ["a.rs", "own/c.rs"]);
    }

    /// A checkout made with `--separate-git-dir` has a `.git` file pointing
    /// at a repository kept elsewhere, and that repository's `info/exclude`
    /// and the `core.excludesFile` its config names are the ones that apply:
    /// `shared` and `configured`, each ignored by one of them alone, are
    /// taken, while `visible` is refused and left empty. A candidate under
    /// the `.git` file cannot be made and is passed over, not taken for the
    /// repository's own directory, and nothing of the separate repository
    /// is listed.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_by_a_separate_git_directory() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path().join("checkout");
        let git_dir = outside.path().join("repo.git");
        std::fs::create_dir(&root).unwrap();
        git_str(
            &root,
            &[
                "init",
                "-q",
                "--separate-git-dir",
                git_dir.to_str().unwrap(),
            ],
        );
        assert!(root.join(".git").is_file());
        std::fs::create_dir_all(git_dir.join("info")).unwrap();
        std::fs::write(git_dir.join("info/exclude"), "/shared/\n").unwrap();
        let rules = outside.path().join("configured.rules");
        std::fs::write(&rules, "/configured/\n").unwrap();
        git_str(
            &root,
            &["config", "core.excludesFile", rules.to_str().unwrap()],
        );
        assert!(
            std::fs::read_to_string(git_dir.join("config"))
                .unwrap()
                .contains("configured.rules")
        );
        let visible = root.join("visible");
        let shared = root.join("shared");
        let configured = root.join("configured");
        for dir in [&visible, &shared, &configured] {
            std::fs::create_dir(dir).unwrap();
        }

        let through_file = root.join(".git/scratch");
        let tmp = tempdir_in_first(&root, [through_file, visible.clone(), shared.clone()]);
        assert_eq!(tmp.path().parent(), Some(shared.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        let tmp_configured = tempdir_in_first(&root, [visible.clone(), configured.clone()]);
        assert_eq!(tmp_configured.path().parent(), Some(configured.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        for dir in [tmp.path(), tmp_configured.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(visible.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(&root, false), ["visible/a.rs"]);
        super::run(&root, false).unwrap();
    }

    /// A `.git` file may name its repository relative to itself, and end
    /// its line in CRLF as an editor on Windows saves it: git reads
    /// `gitdir: ../repo.git` from the checkout's directory, not from where
    /// it runs, and drops the trailing `\r\n`. That repository's
    /// `info/exclude` then ignores `shared`, which is taken, while
    /// `visible` is refused and left empty, and run from a subdirectory the
    /// same rule holds. The repository beside the checkout is never listed.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_may_be_ignored_through_a_relative_git_file() {
        let repo = tempdir();
        let root = repo.path().join("checkout");
        let git_dir = repo.path().join("repo.git");
        std::fs::create_dir(&root).unwrap();
        git_str(
            &root,
            &[
                "init",
                "-q",
                "--separate-git-dir",
                git_dir.to_str().unwrap(),
            ],
        );
        std::fs::write(root.join(".git"), "gitdir: ../repo.git\r\n").unwrap();
        std::fs::create_dir_all(git_dir.join("info")).unwrap();
        std::fs::write(git_dir.join("info/exclude"), "/shared/\n").unwrap();
        let visible = root.join("visible");
        let shared = root.join("shared");
        let sub = root.join("sub");
        for dir in [&visible, &shared, &sub] {
            std::fs::create_dir(dir).unwrap();
        }
        let top = git_str(&root, &["rev-parse", "--absolute-git-dir"]);
        let top = String::from_utf8(top).unwrap();
        assert_eq!(
            std::path::Path::new(top.trim()),
            git_dir.canonicalize().unwrap()
        );
        let from_sub = super::git_command(&root)
            .args([
                "-C",
                "sub",
                "check-ignore",
                "--",
                "../shared/a.rs",
                "../visible/a.rs",
            ])
            .output()
            .unwrap();
        assert_eq!(from_sub.status.code(), Some(0));
        assert_eq!(from_sub.stdout, b"../shared/a.rs\n");

        let tmp = tempdir_in_first(&root, [visible.clone(), shared.clone()]);
        assert_eq!(tmp.path().parent(), Some(shared.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(visible.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(&root, false), ["visible/a.rs"]);
        super::run(&root, false).unwrap();
    }

    /// Ignore sources are opened as git opens them: a UTF-8 byte order mark
    /// at the start of `.gitignore` is dropped, so its first rule ignores
    /// `bom`, which is taken, while the same mark before a later rule is
    /// part of the pattern and `bom2` stays visible and refused. A
    /// `core.excludesFile` naming a file that does not exist is no error and
    /// ignores nothing, so a directory only it was meant to cover is refused
    /// too. `.git/info/exclude`, unlike a `.gitignore`, is read through a
    /// symlink, so a directory named only in the file it leads to is taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_ignore_sources_as_git_opens_them() {
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "\u{feff}/bom/\n\u{feff}/bom2/\n").unwrap();
        let missing = outside.path().join("missing");
        git_str(
            root,
            &["config", "core.excludesFile", missing.to_str().unwrap()],
        );
        let rules = outside.path().join("exclude");
        std::fs::write(&rules, "/linked/\n").unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        let exclude = root.join(".git/info/exclude");
        let _ = std::fs::remove_file(&exclude);
        std::os::unix::fs::symlink(&rules, &exclude).unwrap();
        let bom = root.join("bom");
        let bom2 = root.join("bom2");
        let lost = root.join("missing");
        let linked = root.join("linked");
        for dir in [&bom, &bom2, &lost, &linked] {
            std::fs::create_dir(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [bom2.clone(), lost.clone(), bom.clone()]);
        assert_eq!(tmp.path().parent(), Some(bom.as_path()));
        let tmp_linked = tempdir_in_first(root, [lost.clone(), linked.clone()]);
        assert_eq!(tmp_linked.path().parent(), Some(linked.as_path()));
        assert!(!missing.exists());
        for dir in [&bom2, &lost] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_linked.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(bom2.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(lost.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [".gitignore", "bom2/a.rs", "missing/b.rs"]
        );
        super::run(root, false).unwrap();
    }

    /// An ignore file git cannot read holds no rules: a `.gitignore` made
    /// `000` only draws a warning, and the directory it names stays visible
    /// and is refused, left empty. Nor is a directory named `.gitignore` an
    /// ignore file, so a sibling it was meant to cover is refused too. The
    /// directory a readable rule ignores is taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_without_ignore_files_git_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/kept/\n").unwrap();
        let kept = root.join("kept");
        let locked = root.join("locked/scratch");
        let shaped = root.join("shaped/scratch");
        for dir in [&kept, &locked, &shaped] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let rules = root.join("locked/.gitignore");
        std::fs::write(&rules, "/scratch/\n").unwrap();
        std::fs::create_dir(root.join("shaped/.gitignore")).unwrap();
        std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o000)).unwrap();
        let restore = || {
            std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o644)).unwrap();
        };
        if std::fs::read(&rules).is_ok() {
            // Root, or a capability like it, reads through the mode bits.
            restore();
            return;
        }

        let tmp = tempdir_in_first(root, [locked.clone(), shaped.clone(), kept.clone()]);
        assert_eq!(tmp.path().parent(), Some(kept.as_path()));
        for dir in [&locked, &shaped] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(locked.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(shaped.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [
                ".gitignore",
                "locked/.gitignore",
                "locked/scratch/a.rs",
                "shaped/scratch/b.rs"
            ]
        );
        // Readable again, its rule holds.
        restore();
        assert_eq!(
            names(root, false),
            [".gitignore", "locked/.gitignore", "shaped/scratch/b.rs"]
        );
        super::run(root, false).unwrap();
    }

    /// An exclude file git must read but cannot (`.git/info/exclude`, or a
    /// `core.excludesFile`, that is a directory) is fatal to every git
    /// command that weighs ignore rules, unlike a `.gitignore` of that
    /// shape: `ls-files` fails, and the full check walks the checkout as it
    /// walks a tree outside any repository, skipping only `target` and
    /// `.git`. A TMPDIR is judged as that walk sees it: `ignored`, which
    /// the `.gitignore` names, is refused and left empty, and `target` is
    /// taken. Once the exclude file is a file again, `ignored` is taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_the_walk_when_git_cannot_read_excludes() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/ignored/\n").unwrap();
        let ignored = root.join("ignored");
        let target = root.join("target");
        let elsewhere = root.join(".git/excludes.d");
        for dir in [&ignored, &target, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let exclude = root.join(".git/info/exclude");
        let _ = std::fs::remove_file(&exclude);
        std::fs::create_dir_all(&exclude).unwrap();

        let fails = |args: &[&str]| {
            let out = super::git_command(root).args(args).output().unwrap();
            assert_eq!(out.status.code(), Some(128), "{args:?}");
        };
        let judge = || {
            fails(&["check-ignore", "-q", "--", "ignored"]);
            fails(&["ls-files", "-z", "--others", "--exclude-standard"]);
            let tmp = tempdir_in_first(root, [ignored.clone(), target.clone()]);
            assert_eq!(tmp.path().parent(), Some(target.as_path()));
            assert_eq!(std::fs::read_dir(&ignored).unwrap().count(), 0);
            std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
            let mut walked = names(root, false);
            walked.sort();
            assert_eq!(walked, [".gitignore"]);
            tmp
        };
        let tmp = judge();

        std::fs::remove_dir(&exclude).unwrap();
        git_str(
            root,
            &["config", "core.excludesFile", elsewhere.to_str().unwrap()],
        );
        let tmp_config = judge();

        // `target` is no build directory here, and git lists what is in it.
        drop((tmp, tmp_config));
        git_str(root, &["config", "--unset", "core.excludesFile"]);
        std::fs::write(&exclude, "").unwrap();
        let tmp_ignored = tempdir_in_first(root, [ignored.clone(), target.clone()]);
        assert_eq!(tmp_ignored.path().parent(), Some(ignored.as_path()));
        std::fs::write(tmp_ignored.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(root, false), [".gitignore"]);
        super::run(root, false).unwrap();
    }

    /// An exclude file git can find but not open (`.git/info/exclude`, or a
    /// `core.excludesFile`, made `000`) is no error, unlike one that is a
    /// directory: git only warns, checks every path, and lists untracked
    /// files with no rule from it, so the walk is never needed. `excluded`
    /// and `configured`, named only there, stay visible and are refused,
    /// left empty, while `target`, no build directory here, is refused too,
    /// as git lists it. An empty `core.excludesFile` names no file and
    /// ignores nothing. Readable again, both rules hold and both are taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_without_exclude_files_git_cannot_open() {
        use std::os::unix::fs::PermissionsExt;
        let repo = tempdir();
        let outside = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/own/\n").unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        let exclude = root.join(".git/info/exclude");
        std::fs::write(&exclude, "/excluded/\n").unwrap();
        let rules = outside.path().join("excludes");
        std::fs::write(&rules, "/configured/\n").unwrap();
        git_str(
            root,
            &["config", "core.excludesFile", rules.to_str().unwrap()],
        );
        let excluded = root.join("excluded");
        let configured = root.join("configured");
        let target = root.join("target");
        let own = root.join("own");
        for dir in [&excluded, &configured, &target, &own] {
            std::fs::create_dir(dir).unwrap();
        }
        let mode = |mode| {
            for file in [&exclude, &rules] {
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        };
        mode(0o000);
        if std::fs::read(&exclude).is_ok() {
            // Root, or a capability like it, reads through the mode bits.
            mode(0o644);
            return;
        }

        let out = super::git_command(root)
            .args(["ls-files", "-z", "--others", "--exclude-standard"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("warning"));
        let refused = [excluded.clone(), configured.clone(), target.clone()];
        let tmp = tempdir_in_first(root, refused.iter().cloned().chain([own.clone()]));
        assert_eq!(tmp.path().parent(), Some(own.as_path()));
        for dir in &refused {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(excluded.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(configured.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [".gitignore", "configured/b.rs", "excluded/a.rs"]
        );

        // An empty setting is no file at all, and no error either.
        mode(0o644);
        git_str(root, &["config", "core.excludesFile", ""]);
        assert_eq!(names(root, false), [".gitignore", "configured/b.rs"]);
        let tmp_excluded = tempdir_in_first(root, [configured.clone(), excluded.clone()]);
        assert_eq!(tmp_excluded.path().parent(), Some(excluded.as_path()));

        // Readable and named again, both rules hold.
        git_str(
            root,
            &["config", "core.excludesFile", rules.to_str().unwrap()],
        );
        let tmp_configured = tempdir_in_first(root, [target.clone(), configured.clone()]);
        assert_eq!(tmp_configured.path().parent(), Some(configured.as_path()));
        assert_eq!(names(root, false), [".gitignore"]);
        super::run(root, false).unwrap();
    }

    /// A tracked `.gitignore` is read from disk, not from `HEAD` or the
    /// index: rewritten there and left unstaged, its rule for `fresh` holds
    /// and `fresh` is taken, while `stale`, named only in the commit, and
    /// `indexed`, named only in the staged copy, are refused. Once a
    /// `.gitignore` is outside a sparse checkout (`--skip-worktree`) and
    /// gone from disk, git reads it from the index instead, so `sub/kept`,
    /// named only there, is taken; it is still listed and read as tracked.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_the_ignore_files_git_has_at_hand() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join(".gitignore"), "/stale/\n").unwrap();
        std::fs::write(root.join("sub/.gitignore"), "/kept/\n").unwrap();
        git_str(root, &["add", "--", ".gitignore", "sub/.gitignore"]);
        git_str(root, &["commit", "-q", "--no-verify", "-m", "rules"]);
        std::fs::write(root.join(".gitignore"), "/indexed/\n").unwrap();
        git_str(root, &["add", "--", ".gitignore"]);
        std::fs::write(root.join(".gitignore"), "/fresh/\n").unwrap();
        git_str(root, &["update-index", "--skip-worktree", "sub/.gitignore"]);
        std::fs::remove_file(root.join("sub/.gitignore")).unwrap();
        let stale = root.join("stale");
        let indexed = root.join("indexed");
        let fresh = root.join("fresh");
        let kept = root.join("sub/kept");
        for dir in [&stale, &indexed, &fresh, &kept] {
            std::fs::create_dir(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [stale.clone(), indexed.clone(), fresh.clone()]);
        assert_eq!(tmp.path().parent(), Some(fresh.as_path()));
        let tmp_kept = tempdir_in_first(root, [stale.clone(), kept.clone()]);
        assert_eq!(tmp_kept.path().parent(), Some(kept.as_path()));
        for dir in [&stale, &indexed] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for dir in [tmp.path(), tmp_kept.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(stale.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(indexed.join("b.rs"), b"fn b() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [".gitignore", "indexed/b.rs", "stale/a.rs", "sub/.gitignore"]
        );
        super::run(root, false).unwrap();
    }

    /// A rule is anchored as `.gitignore` syntax places it: `loose`, with
    /// no slash, ignores a directory of that name at any depth, so
    /// `a/b/loose` is taken; `mid/dle`, with a slash before its end, is
    /// anchored to the `.gitignore` holding it, so `x/mid/dle` stays visible
    /// and is refused, while `**/far/off` reaches any depth and `x/y/far/off`
    /// is taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_where_their_rules_are_anchored() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "loose\nmid/dle\n**/far/off\n").unwrap();
        let loose = root.join("a/b/loose");
        let unanchored = root.join("x/mid/dle");
        let far = root.join("x/y/far/off");
        for dir in [&loose, &unanchored, &far] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [unanchored.clone(), loose.clone()]);
        assert_eq!(tmp.path().parent(), Some(loose.as_path()));
        let tmp_far = tempdir_in_first(root, [unanchored.clone(), far.clone()]);
        assert_eq!(tmp_far.path().parent(), Some(far.as_path()));
        assert_eq!(std::fs::read_dir(&unanchored).unwrap().count(), 0);
        for dir in [tmp.path(), tmp_far.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(unanchored.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), [".gitignore", "x/mid/dle/a.rs"]);
        super::run(root, false).unwrap();
    }

    /// A rule's wildcards are read as `.gitignore` globs: `?` and `*` match
    /// within one path component and never across `/`, `[st]` and `[!x]`
    /// match one character in or out of a class, and `\*` matches only a
    /// literal star. So `scratch`, `tmp`, `temp`, `star*` and `one/two/deep`
    /// are taken, while `scr/tch`, `ump`, `xemp`, `starry` and
    /// `one/two/three/deep`, each one character or component off, stay
    /// visible and are refused.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_by_the_globs_their_rules_spell() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "/scr?tch/\n/[st]mp/\n/[!x]emp/\n/star\\*/\n/one/*/deep/\n",
        )
        .unwrap();
        let taken = ["scratch", "tmp", "temp", "star*", "one/two/deep"].map(|d| root.join(d));
        let refused =
            ["scr/tch", "ump", "xemp", "starry", "one/two/three/deep"].map(|d| root.join(d));
        for dir in taken.iter().chain(&refused) {
            std::fs::create_dir_all(dir).unwrap();
        }

        let mut tmps = Vec::new();
        for (seen, hidden) in refused.iter().zip(&taken) {
            let tmp = tempdir_in_first(root, [seen.clone(), hidden.clone()]);
            assert_eq!(tmp.path().parent(), Some(hidden.as_path()));
            assert_eq!(std::fs::read_dir(seen).unwrap().count(), 0);
            std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
            tmps.push(tmp);
        }
        for (i, dir) in refused.iter().enumerate() {
            std::fs::write(dir.join(format!("f{i}.rs")), b"fn f() {}\n").unwrap();
        }
        assert_eq!(
            names(root, false),
            [
                ".gitignore",
                "one/two/three/deep/f4.rs",
                "scr/tch/f0.rs",
                "starry/f3.rs",
                "ump/f1.rs",
                "xemp/f2.rs",
            ]
        );
        super::run(root, false).unwrap();
    }

    /// A `/**/` between two components matches zero or more directories,
    /// so `/a/**/deep/` takes both `a/deep` and `a/b/c/deep`, but never a
    /// partial component: `ab/deep` stays visible. A range `[0-9]` takes
    /// `run7` and refuses `runx`. Asterisks not set off by slashes are
    /// plain `*`, so `/x**y/` takes `xzzy` but never spans `x/y`.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_by_double_stars_and_ranges() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/a/**/deep/\n/run[0-9]/\n/x**y/\n").unwrap();
        let taken = ["a/deep", "a/b/c/deep", "run7", "xzzy"].map(|d| root.join(d));
        let refused = ["ab/deep", "ab/deep", "runx", "x/y"].map(|d| root.join(d));
        for dir in taken.iter().chain(&refused) {
            std::fs::create_dir_all(dir).unwrap();
        }

        let mut tmps = Vec::new();
        for (seen, hidden) in refused.iter().zip(&taken) {
            let tmp = tempdir_in_first(root, [seen.clone(), hidden.clone()]);
            assert_eq!(tmp.path().parent(), Some(hidden.as_path()));
            assert_eq!(std::fs::read_dir(seen).unwrap().count(), 0);
            std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
            tmps.push(tmp);
        }
        for dir in ["ab/deep", "runx", "x/y"] {
            std::fs::write(root.join(dir).join("f.rs"), b"fn f() {}\n").unwrap();
        }
        assert_eq!(
            names(root, false),
            [".gitignore", "ab/deep/f.rs", "runx/f.rs", "x/y/f.rs"]
        );
        super::run(root, false).unwrap();
    }

    /// Bracket expressions and escapes are read as git's wildmatch reads
    /// them: `[[:digit:]]` is a POSIX class, taking `run7` and refusing
    /// `runx`; a `]` first in a bracket is literal, so `[]]x` takes `]x`; a
    /// `-` last in one is literal, so `[a-]z` takes `-z` and refuses `bz`;
    /// `\?` matches only a literal `?`, taking `a?b` and refusing `acb`. A
    /// pattern ending in a lone backslash matches nothing, so `odd\` stays
    /// visible and is refused. The last rule, with no newline after it,
    /// still holds, and `nonl` is taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_by_bracket_classes_and_escapes() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "/run[[:digit:]]/\n/[]]x/\n/[a-]z/\n/a\\?b/\n/odd\\\n/nonl/",
        )
        .unwrap();
        let taken = ["run7", "]x", "-z", "a?b", "nonl"].map(|d| root.join(d));
        let refused = ["runx", "bz", "acb", "odd\\"].map(|d| root.join(d));
        for dir in taken.iter().chain(&refused) {
            std::fs::create_dir(dir).unwrap();
        }

        let tmps: Vec<_> = taken
            .iter()
            .map(|hidden| {
                let mut candidates = refused.to_vec();
                candidates.push(hidden.clone());
                let tmp = tempdir_in_first(root, candidates);
                assert_eq!(tmp.path().parent(), Some(hidden.as_path()));
                std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
                tmp
            })
            .collect();
        assert_eq!(tmps.len(), taken.len());
        for dir in &refused {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for (i, dir) in refused.iter().enumerate() {
            std::fs::write(dir.join(format!("f{i}.rs")), b"fn f() {}\n").unwrap();
        }
        assert_eq!(
            names(root, false),
            [
                ".gitignore",
                "acb/f2.rs",
                "bz/f1.rs",
                "odd\\/f3.rs",
                "runx/f0.rs"
            ]
        );
        super::run(root, false).unwrap();
    }

    /// A rule's metacharacters are never also read literally: `/[ab]/`
    /// takes `a` but leaves a directory named `[ab]` visible, so that is
    /// refused, while the escaped `/\[cd]/` takes `[cd]` and refuses `c`; a
    /// `*` matches itself as well as anything else, so `/q*z/` takes `q*z`
    /// and `qxz`. Bracket classes and ranges are case-sensitive until
    /// `core.ignoreCase=true`: `/[[:upper:]]up/` and `/[A-C]rng/` take `Xup`
    /// and `Arng` but refuse `yup` and `crng`, which they take once case is
    /// ignored; `[ab]` and `c` are refused either way.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_matched_by_metacharacters_never_literally() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        git_str(root, &["config", "core.ignoreCase", "false"]);
        std::fs::write(
            root.join(".gitignore"),
            "/[ab]/\n/\\[cd]/\n/q*z/\n/[[:upper:]]up/\n/[A-C]rng/\n",
        )
        .unwrap();
        let taken = ["a", "[cd]", "q*z", "qxz", "Xup", "Arng"].map(|d| root.join(d));
        let refused = ["[ab]", "c"].map(|d| root.join(d));
        let cased = ["yup", "crng"].map(|d| root.join(d));
        for dir in taken.iter().chain(&refused).chain(&cased) {
            std::fs::create_dir(dir).unwrap();
        }

        let in_first = |hidden: &std::path::PathBuf, also_refused: &[std::path::PathBuf]| {
            let mut candidates = refused.to_vec();
            candidates.extend_from_slice(also_refused);
            candidates.push(hidden.clone());
            let tmp = tempdir_in_first(root, candidates);
            assert_eq!(tmp.path().parent(), Some(hidden.as_path()));
            std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
            tmp
        };
        let mut tmps: Vec<_> = taken.iter().map(|d| in_first(d, &cased)).collect();
        for dir in refused.iter().chain(&cased) {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }

        git_str(root, &["config", "core.ignoreCase", "true"]);
        tmps.extend(cased.iter().map(|d| in_first(d, &[])));
        assert_eq!(tmps.len(), taken.len() + cased.len());
        for dir in &refused {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        for (i, dir) in refused.iter().enumerate() {
            std::fs::write(dir.join(format!("f{i}.rs")), b"fn f() {}\n").unwrap();
        }
        assert_eq!(names(root, false), [".gitignore", "[ab]/f0.rs", "c/f1.rs"]);
        super::run(root, false).unwrap();
    }

    /// Ignoring every file under a directory while re-including the
    /// directories in it (`/layout/**` then `!/layout/**/`, the idiom that
    /// keeps a tree's shape and none of its files) leaves those directories
    /// visible, so a TMPDIR there is refused and left empty, though no
    /// plain file made in it would be listed: the temporary repository a
    /// test makes there is a directory holding `.git`, which git lists as a
    /// nested repository, and that fails the full check. A later
    /// `!/layout/**/*.rs` brings `.rs` files back, which are listed as well.
    /// The directory a plain `/hidden/` ignores is taken.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_under_reincluded_directories_are_refused() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "/layout/**\n!/layout/**/\n!/layout/**/*.rs\n/hidden/\n",
        )
        .unwrap();
        let layout = root.join("layout");
        let hidden = root.join("hidden");
        for dir in [&layout, &hidden] {
            std::fs::create_dir(dir).unwrap();
        }

        let tmp = tempdir_in_first(root, [layout.clone(), hidden.clone()]);
        assert_eq!(tmp.path().parent(), Some(hidden.as_path()));
        assert_eq!(std::fs::read_dir(&layout).unwrap().count(), 0);
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();

        std::fs::create_dir_all(layout.join("sub")).unwrap();
        std::fs::write(layout.join("sub/game.bin"), b"MZ\x90\0fixture").unwrap();
        std::fs::write(layout.join("sub/f.rs"), b"fn f() {}\n").unwrap();
        assert_eq!(names(root, false), [".gitignore", "layout/sub/f.rs"]);
        super::run(root, false).unwrap();

        let made = layout.join("made");
        std::fs::create_dir(&made).unwrap();
        git_str(&made, &["init", "-q"]);
        std::fs::write(made.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(
            names(root, false),
            [".gitignore", "layout/made/", "layout/sub/f.rs"]
        );
        assert!(super::run(root, false).is_err());
    }

    /// A TMPDIR spelled with `.` or `..` is judged by the directory it
    /// names, not by its spelling: `visible/../ignored` and `ignored/.` are
    /// taken in `ignored`, while `ignored/../visible`, which passes through
    /// the ignored directory on its way, leads to `visible` and is refused,
    /// as is `ignored/../visible/.`. Nothing is made in `visible`.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_the_directory_their_path_names() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/ignored/\n").unwrap();
        let ignored = root.join("ignored");
        let visible = root.join("visible");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::create_dir(&visible).unwrap();
        let through = root.join("ignored/../visible");
        let through_dot = root.join("ignored/../visible/.");

        let tmp = tempdir_in_first(
            root,
            [
                through.clone(),
                through_dot.clone(),
                root.join("visible/../ignored"),
            ],
        );
        assert_eq!(tmp.path().parent(), Some(ignored.as_path()));
        let tmp_dot = tempdir_in_first(root, [through, through_dot, root.join("ignored/.")]);
        assert_eq!(tmp_dot.path().parent(), Some(ignored.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        for dir in [tmp.path(), tmp_dot.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(visible.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(names(root, false), [".gitignore", "visible/a.rs"]);
        super::run(root, false).unwrap();
    }

    /// A TMPDIR reached through symlinks inside the checkout is judged by
    /// the directory it leads to, not by the link's own name or by any
    /// rule naming the link: `alias.txt` and `hop.txt` (a link to it) lead
    /// to the ignored `ignored` and are taken there, while `named.txt`, a link the
    /// top `.gitignore` ignores by name, leads to `visible` and is refused,
    /// as is `seen.txt`, a plain link to it. The links stay links, and nothing
    /// is made in `visible`.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_follow_links_within_the_checkout() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/ignored/\n/named.txt\n").unwrap();
        let ignored = root.join("ignored");
        let visible = root.join("visible");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::create_dir(&visible).unwrap();
        let link = |name: &str, to: &str| {
            let path = root.join(name);
            std::os::unix::fs::symlink(to, &path).unwrap();
            path
        };
        let alias = link("alias.txt", "ignored");
        let hop = link("hop.txt", "alias.txt");
        let named = link("named.txt", "visible");
        let seen = link("seen.txt", "visible");

        let tmp = tempdir_in_first(root, [named.clone(), seen.clone(), alias.clone()]);
        assert_eq!(tmp.path().parent(), Some(ignored.as_path()));
        let tmp_hop = tempdir_in_first(root, [seen.clone(), hop.clone()]);
        assert_eq!(tmp_hop.path().parent(), Some(ignored.as_path()));
        assert_eq!(std::fs::read_dir(&visible).unwrap().count(), 0);
        for path in [&alias, &hop, &named, &seen] {
            assert!(path.symlink_metadata().unwrap().is_symlink());
        }
        for dir in [tmp.path(), tmp_hop.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        std::fs::write(visible.join("a.rs"), b"fn a() {}\n").unwrap();
        assert_eq!(
            names(root, false),
            [
                ".gitignore",
                "alias.txt",
                "hop.txt",
                "seen.txt",
                "visible/a.rs"
            ]
        );
        super::run(root, false).unwrap();
    }

    /// Git lists an untracked repository nested in the checkout as the one
    /// entry `inner/`, whatever it holds, so a TMPDIR there adds nothing to
    /// the listing; it is still judged by the ignore rules as `check-ignore`
    /// reads them. `inner/vis`, which no rule covers, is refused and left
    /// empty; `inner/ign`, ignored by the outer `.gitignore`, and
    /// `inner/own`, ignored only by the nested repository's own
    /// `.gitignore`, are taken. The nested repository is refused either way,
    /// fixtures or not.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_in_a_nested_repository_add_nothing_to_the_listing() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/inner/ign/\n").unwrap();
        let inner = root.join("inner");
        let vis = inner.join("vis");
        let ign = inner.join("ign");
        let own = inner.join("own");
        for dir in [&vis, &ign, &own] {
            std::fs::create_dir_all(dir).unwrap();
        }
        git_str(&inner, &["init", "-q"]);
        std::fs::write(inner.join(".gitignore"), "/own/\n").unwrap();
        let before = names(root, false);
        assert_eq!(before, [".gitignore", "inner/"]);

        let tmp = tempdir_in_first(root, [vis.clone(), ign.clone()]);
        assert_eq!(tmp.path().parent(), Some(ign.as_path()));
        let tmp_own = tempdir_in_first(root, [vis.clone(), own.clone()]);
        assert_eq!(tmp_own.path().parent(), Some(own.as_path()));
        assert_eq!(std::fs::read_dir(&vis).unwrap().count(), 0);
        for dir in [tmp.path(), tmp_own.path()] {
            std::fs::write(dir.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        }
        assert_eq!(names(root, false), before);
        assert!(super::run(root, false).is_err());
    }

    /// Git reads no ignore rule from above the checkout: a repository
    /// holding it, though its `.gitignore` and `info/exclude` name the
    /// checkout's directories, hides none of them, so `above` and `excluded`
    /// are refused and left empty, and only `own`, ignored by the checkout's
    /// own `.gitignore`, is taken. A fixture in a refused directory is
    /// listed and fails the check.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_are_judged_by_no_rule_above_the_checkout() {
        let outer = tempdir();
        let parent = outer.path();
        git_str(parent, &["init", "-q"]);
        std::fs::write(parent.join(".gitignore"), "/repo/above/\nabove/\n*.bin\n").unwrap();
        std::fs::create_dir_all(parent.join(".git/info")).unwrap();
        std::fs::write(parent.join(".git/info/exclude"), "/repo/excluded/\n").unwrap();
        let root = parent.join("repo");
        let above = root.join("above");
        let excluded = root.join("excluded");
        let own = root.join("own");
        for dir in [&above, &excluded, &own] {
            std::fs::create_dir_all(dir).unwrap();
        }
        git_str(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "/own/\n").unwrap();

        let tmp = tempdir_in_first(&root, [above.clone(), excluded.clone(), own.clone()]);
        assert_eq!(tmp.path().parent(), Some(own.as_path()));
        for dir in [&above, &excluded] {
            assert_eq!(std::fs::read_dir(dir).unwrap().count(), 0);
        }
        std::fs::write(tmp.path().join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(&root, false), [".gitignore"]);
        super::run(&root, false).unwrap();

        std::fs::write(above.join("game.bin"), b"MZ\x90\0fixture").unwrap();
        assert_eq!(names(&root, false), [".gitignore", "above/game.bin"]);
        assert!(super::run(&root, false).is_err());
    }

    /// A CARGO_TARGET_DIR that is the checkout itself is never passed over,
    /// whatever its tag says: the whole tree would go unchecked. What is
    /// built there is listed, in a repository and outside one, and passing
    /// over is left to `.gitignore` as for any other file.
    #[cfg(unix)]
    #[test]
    fn a_build_directory_at_the_top_of_the_checkout_is_still_read() {
        let repo = tempdir();
        let root = repo.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(root);
        assert_eq!(super::build_dir_in(root, &exe), None);
        assert!(!super::leads_to_build_dir(root, &exe));
        let listed = names_built_by(root, &exe);
        for name in [
            "a.rs",
            "CACHEDIR.TAG",
            "debug/main.o",
            "debug/deps/xtask-0123",
        ] {
            assert!(listed.contains(&name.to_owned()), "{name}: {listed:?}");
        }
        std::fs::write(root.join(".gitignore"), "/debug/\n").unwrap();
        assert_eq!(
            names_built_by(root, &exe),
            [".gitignore", "CACHEDIR.TAG", "a.rs"]
        );

        let outer = tempdir();
        let plain = outer.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("a.rs"), b"fn a() {}\n").unwrap();
        let exe = fake_build_dir(&plain);
        let listed = names_built_by(&plain, &exe);
        assert!(listed.contains(&"debug/main.o".to_owned()), "{listed:?}");
    }

    /// `path` quoted for `sh`, for the scripts tests write: a TMPDIR holding
    /// `'` made the hook and filter commands built from it shell syntax
    /// errors, and the git commands running them failed.
    #[cfg(unix)]
    fn sh_quote(path: &std::path::Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }

    /// Whether nothing under `dir` is listed by the full check of `checkout`:
    /// it is elsewhere, inside a `.git` directory, in the build directory the
    /// tests run from, or ignored there.
    ///
    /// Git never lists what is under `.git`, and the walk skips it too, but
    /// `check-ignore` calls such a path not ignored: a TMPDIR kept in the
    /// checkout's `.git` (to stay on its filesystem, or out of `/tmp`) was
    /// passed over for beside the test binary, and when that was not
    /// writable every test panicked asking for the TMPDIR it already had.
    fn unseen_by_full_check(checkout: &std::path::Path, dir: &std::path::Path) -> bool {
        let Ok(relative) = dir.strip_prefix(checkout) else {
            return true;
        };
        if relative.components().any(|c| c.as_os_str() == ".git") {
            return true;
        }
        // The build directory the tests run from is passed over by the full
        // check wherever it is (see `build_dir_in`), ignored or not.
        let build = std::env::current_exe()
            .ok()
            .and_then(|exe| super::build_dir_in(checkout, &exe));
        let raw: Vec<u8> = relative
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|&b| if cfg!(windows) && b == b'\\' { b'/' } else { b })
            .collect();
        if super::is_under(&raw, build.as_deref()) {
            return true;
        }
        let status = super::git_command(checkout)
            .args(["check-ignore", "-q", "--"])
            .arg(relative)
            .status();
        match status.ok().and_then(|s| s.code()) {
            Some(0) => true,
            Some(1) => false,
            // Not a git checkout: the walk skips only `target` and `.git`.
            _ => relative
                .components()
                .any(|c| matches!(c.as_os_str().to_str(), Some("target" | ".git"))),
        }
    }

    #[cfg(unix)]
    fn names(root: &std::path::Path, staged: bool) -> Vec<String> {
        super::list_files(root, staged)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect()
    }

    /// On Unix a `\` is part of a file's name, not a separator, and the
    /// name is listed and judged as it is: `tool.exe\LICENSE` is one file
    /// whose name ends in `.exe\LICENSE`, not `LICENSE` in a directory
    /// `tool.exe`, so it is off the allowlist in the full and staged checks
    /// alike, though its content is plain text.
    #[cfg(unix)]
    #[test]
    fn backslashes_are_kept_in_unix_names() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(root.join("tool.exe\\LICENSE"), b"MIT\n").unwrap();

        assert_eq!(names(root, false), ["a.rs", "tool.exe\\LICENSE"]);
        assert!(super::run(root, false).is_err());
        git_str(root, &["add", "--", "a.rs", "tool.exe\\LICENSE"]);
        assert_eq!(names(root, true), ["a.rs", "tool.exe\\LICENSE"]);
        assert!(super::run(root, true).is_err());
        assert_eq!(
            check("tool.exe\\LICENSE", b"MIT\n"),
            ["file type not on the allowlist"]
        );
        assert!(check("tool.exe/LICENSE", b"MIT\n").is_empty());
    }

    /// A symlink turned into a regular file is staged as a type change (`T`),
    /// not a modification; the pre-commit check must still read it.
    #[cfg(unix)]
    #[test]
    fn staged_check_reads_type_changes() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::os::unix::fs::symlink("elsewhere.rs", root.join("data.rs")).unwrap();
        git_str(root, &["add", "data.rs"]);
        git_str(root, &["commit", "-q", "--no-verify", "-m", "link"]);
        std::fs::remove_file(root.join("data.rs")).unwrap();
        std::fs::write(root.join("data.rs"), b"MZ\x90\0binary").unwrap();
        git_str(root, &["add", "data.rs"]);

        assert_eq!(names(root, true), ["data.rs"]);
        assert!(super::run(root, true).is_err());
    }

    /// Replace refs stay local, so the hook must not read through them: a
    /// staged blob replaced by harmless text, or a HEAD replaced by a commit
    /// whose tree already holds the staged file, used to pass.
    #[cfg(unix)]
    #[test]
    fn staged_check_ignores_replace_refs() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        git_str(root, &["add", "a.rs"]);
        git_str(root, &["commit", "-q", "--no-verify", "-m", "a"]);
        std::fs::write(root.join("b.rs"), b"MZ\x90\0binary").unwrap();
        git_str(root, &["add", "b.rs"]);

        let staged = git_str(root, &["rev-parse", ":b.rs"]);
        let staged = String::from_utf8(staged).unwrap();
        std::fs::write(root.join("ok.rs"), b"fn ok() {}\n").unwrap();
        let harmless = git_str(root, &["hash-object", "-w", "ok.rs"]);
        let harmless = String::from_utf8(harmless).unwrap();
        std::fs::remove_file(root.join("ok.rs")).unwrap();
        git_str(root, &["replace", staged.trim(), harmless.trim()]);
        assert!(super::run(root, true).is_err(), "replaced blob");
        git_str(root, &["replace", "-d", staged.trim()]);

        let tree = git_str(root, &["write-tree"]);
        let tree = String::from_utf8(tree).unwrap();
        let fake = git_str(root, &["commit-tree", tree.trim(), "-p", "HEAD", "-m", "x"]);
        let fake = String::from_utf8(fake).unwrap();
        git_str(root, &["replace", "HEAD", fake.trim()]);
        assert_eq!(names(root, true), ["b.rs"]);
        assert!(super::run(root, true).is_err(), "replaced HEAD");
    }

    /// Git stores a symlink as its target path. The full check used to
    /// follow the link instead: it refused a link to a binary outside the
    /// repository that the hook had accepted, passed a dangling link whose
    /// path the hook refuses, and would block on a link to a FIFO.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_checked_as_git_stores_them() {
        let outside = tempdir();
        let binary = outside.path().join("game.bin");
        std::fs::write(&binary, b"MZ\x90\0binary").unwrap();
        let fifo = outside.path().join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.unwrap().success(), "mkfifo");

        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::os::unix::fs::symlink(&binary, root.join("binary.rs")).unwrap();
        std::os::unix::fs::symlink(&fifo, root.join("fifo.rs")).unwrap();
        git_str(root, &["add", "binary.rs", "fifo.rs"]);
        super::run(root, true).unwrap();
        super::run(root, false).unwrap();

        // A path that is itself a long base64-like run, pointing nowhere.
        let target = format!("{}/x", "a".repeat(super::MAX_ENCODED_RUN));
        std::os::unix::fs::symlink(&target, root.join("dangling.rs")).unwrap();
        let files = super::list_files(root, false).unwrap();
        let dangling = files.iter().find(|f| f.name == "dangling.rs").unwrap();
        assert_eq!(
            super::read(root, dangling).unwrap().as_deref(),
            Some(target.as_bytes())
        );
        assert!(super::run(root, false).is_err());
        git_str(root, &["add", "dangling.rs"]);
        assert!(super::run(root, true).is_err());
    }

    /// An exception covers the content it was granted for, not whatever is
    /// later committed under its name.
    #[test]
    fn exceptions_are_pinned_to_their_content() {
        assert!(super::exempt("src/lib.rs", b"fn main() {}\n").is_none());
        for (path, pinned, _) in super::EXCEPTIONS {
            assert_eq!(pinned.len(), 64, "{path}: pin a full BLAKE3 hex digest");
            let err = super::exempt(path, b"MZ\x90\0anything").unwrap();
            assert!(err.is_err(), "{path}: other content passed");
            assert!(super::exempt(path, b"").unwrap().is_err());
        }
    }

    /// The full and staged checks both refuse new content at an exempted
    /// path, which used to pass whatever it held.
    #[cfg(unix)]
    #[test]
    fn exempted_paths_refuse_other_content() {
        let (path, ..) = super::EXCEPTIONS[0];
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"MZ\x90\0a game executable").unwrap();
        git_str(root, &["add", path]);
        assert!(super::run(root, false).is_err());
        assert!(super::run(root, true).is_err());

        let real = crate::workspace_root().join(path);
        if let Ok(content) = std::fs::read(real) {
            std::fs::write(&file, content).unwrap();
            git_str(root, &["add", path]);
            super::run(root, false).unwrap();
            super::run(root, true).unwrap();
        }
    }

    /// A renamed file is checked under its new name, with its staged content.
    #[cfg(unix)]
    #[test]
    fn staged_check_reads_renames() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        git_str(root, &["add", "a.rs"]);
        git_str(root, &["commit", "-q", "--no-verify", "-m", "a"]);
        git_str(root, &["mv", "a.rs", "b.rs"]);
        assert_eq!(names(root, true), ["b.rs"]);
        super::run(root, true).unwrap();
    }

    /// A file whose name is not UTF-8 used to be read back under its lossily
    /// decoded name, which does not exist, and so passed unread.
    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_violations() {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let mut hash = super::git_command(root)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        hash.stdin.take().unwrap().write_all(b"MZ\x90\0").unwrap();
        let out = hash.wait_with_output().unwrap();
        let oid = String::from_utf8(out.stdout).unwrap();
        // Staged straight into the index: macOS refuses such names on disk.
        let cacheinfo = format!("100644,{},", oid.trim()).into_bytes();
        let cacheinfo = [cacheinfo.as_slice(), b"data\xff.rs"].concat();
        git(
            root,
            &[
                "update-index".as_ref(),
                "--add".as_ref(),
                "--cacheinfo".as_ref(),
                std::ffi::OsStr::from_bytes(&cacheinfo),
            ],
        );

        let staged = super::list_files(root, true).unwrap();
        assert_eq!(staged.len(), 1);
        assert!(!staged[0].utf8_name);
        assert_eq!(
            super::read(root, &staged[0]).unwrap().as_deref(),
            Some(&b"MZ\x90\0"[..]),
            "staged content is read by object id"
        );
        assert!(super::run(root, true).is_err());
    }

    /// The same, for a file on disk (Linux file systems allow such names).
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_names_on_disk_are_read() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let name = std::ffi::OsStr::from_bytes(b"data\xff.rs");
        std::fs::write(root.join(name), b"MZ\x90\0").unwrap();

        let files = super::list_files(root, false).unwrap();
        assert_eq!(files.len(), 1);
        assert!(!files[0].utf8_name);
        assert_eq!(
            super::read(root, &files[0]).unwrap().as_deref(),
            Some(&b"MZ\x90\0"[..])
        );
        assert!(super::run(root, false).is_err());
    }

    /// A tracked file missing from disk is still in the index, and a plain
    /// `git commit` keeps it; the full check reads it from there. Committed
    /// with `--no-verify` and then deleted, or hidden from a sparse checkout,
    /// it used to pass unread.
    #[cfg(unix)]
    #[test]
    fn tracked_files_missing_from_disk_are_read_from_the_index() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(root.join("game.rs"), b"MZ\x90\0binary").unwrap();
        git_str(root, &["add", "a.rs", "game.rs"]);
        git_str(root, &["commit", "-q", "--no-verify", "-m", "x"]);

        std::fs::remove_file(root.join("game.rs")).unwrap();
        assert_eq!(names(root, false), ["a.rs", "game.rs"]);
        assert!(super::run(root, false).is_err(), "deleted, not staged");

        git_str(root, &["update-index", "--skip-worktree", "game.rs"]);
        assert!(
            super::run(root, false).is_err(),
            "outside the sparse checkout"
        );
        git_str(root, &["update-index", "--no-skip-worktree", "game.rs"]);

        // Removed from the index too: nothing of it is left to check.
        git_str(root, &["rm", "-q", "--cached", "game.rs"]);
        assert_eq!(names(root, false), ["a.rs"]);
        super::run(root, false).unwrap();

        // A deleted symlink is read as the target path git stores.
        let target = format!("{}/x", "a".repeat(super::MAX_ENCODED_RUN));
        std::os::unix::fs::symlink(&target, root.join("link.rs")).unwrap();
        git_str(root, &["add", "link.rs"]);
        std::fs::remove_file(root.join("link.rs")).unwrap();
        let files = super::list_files(root, false).unwrap();
        let link = files.iter().find(|f| f.name == "link.rs").unwrap();
        assert_eq!(
            super::read(root, link).unwrap().as_deref(),
            Some(target.as_bytes())
        );
    }

    /// The full check reads what the index holds as well as the working
    /// copy: a binary committed with `--no-verify` and then overwritten on
    /// disk with text stays in the repository, and `--assume-unchanged` or
    /// `--skip-worktree` hide it from `git status` too.
    #[cfg(unix)]
    #[test]
    fn tracked_files_are_read_as_indexed_as_well() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(root.join("game.rs"), b"MZ\x90\0binary").unwrap();
        git_str(root, &["add", "a.rs", "game.rs"]);
        git_str(root, &["commit", "-q", "--no-verify", "-m", "x"]);

        std::fs::write(root.join("game.rs"), b"fn game() {}\n").unwrap();
        assert!(super::run(root, false).is_err(), "edited on disk");
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            git_str(root, &["update-index", flag, "game.rs"]);
            assert!(super::run(root, false).is_err(), "{flag}");
            git_str(
                root,
                &["update-index", &flag.replace("--", "--no-"), "game.rs"],
            );
        }
        let indexed = super::indexed_blobs(root, &super::list_files(root, false).unwrap()).unwrap();
        assert_eq!(indexed.len(), 2);
        assert!(
            indexed
                .values()
                .any(|b| b.as_deref() == Some(b"MZ\x90\0binary"))
        );

        // Staged, the index holds the text too, and both checks pass.
        git_str(root, &["add", "game.rs"]);
        super::run(root, false).unwrap();
        super::run(root, true).unwrap();

        // Text on disk over text in the index is read once.
        std::fs::write(root.join("a.rs"), b"fn a() { 1 }\n").unwrap();
        super::run(root, false).unwrap();
    }

    /// A file over the limit is read no further than the limit, on disk, in
    /// the index and when staged, and is refused with its whole size.
    #[cfg(unix)]
    #[test]
    fn files_over_the_limit_are_not_read_whole() {
        const BIG: u64 = 8 << 20;
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        // Sparse, so the test writes almost nothing to disk.
        std::fs::File::create(root.join("game.rs"))
            .unwrap()
            .set_len(BIG)
            .unwrap();
        let read = |staged: bool| {
            let files = super::list_files(root, staged).unwrap();
            assert_eq!(files.len(), 1);
            let content = super::read(root, &files[0]).unwrap().unwrap();
            assert_eq!(content.size, BIG);
            assert_eq!(content.head.len(), super::MAX_FILE_SIZE);
        };
        read(false);
        assert!(super::run(root, false).is_err(), "untracked");

        git_str(root, &["add", "game.rs"]);
        read(true);
        assert!(super::run(root, true).is_err(), "staged");
        std::fs::write(root.join("game.rs"), b"fn game() {}\n").unwrap();
        let files = super::list_files(root, false).unwrap();
        let indexed = super::indexed_blobs(root, &files).unwrap();
        let blob = indexed.values().next().unwrap().as_ref().unwrap();
        assert_eq!((blob.size, blob.head.len()), (BIG, super::MAX_FILE_SIZE));
        assert!(super::run(root, false).is_err(), "as indexed");

        // At the limit exactly, the whole file is read and nothing is cut.
        let edge = vec![b'\n'; super::MAX_FILE_SIZE];
        let content = super::Content::read(&edge[..]).unwrap();
        assert_eq!(
            (content.size, content.head.len()),
            (edge.len() as u64, edge.len())
        );
        std::fs::write(root.join("game.rs"), &edge).unwrap();
        git_str(root, &["add", "game.rs"]);
        super::run(root, false).unwrap();
        super::run(root, true).unwrap();
    }

    /// A conflicted path has one index entry per stage, and the full check
    /// reads every one of them, not only the last: `git checkout --ours`
    /// puts stage 2 back in the next commit whatever the working copy and
    /// stage 3 hold.
    #[cfg(unix)]
    #[test]
    fn every_stage_of_a_conflicted_path_is_read() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let blob = |content: &[u8]| {
            std::fs::write(root.join("blob"), content).unwrap();
            let oid = git_str(root, &["hash-object", "-w", "blob"]);
            std::fs::remove_file(root.join("blob")).unwrap();
            String::from_utf8(oid).unwrap().trim().to_owned()
        };
        let clean = blob(b"fn game() {}\n");
        let game = blob(b"MZ\x90\0binary");
        let conflict = |stages: [&str; 3]| {
            let info: String = stages
                .iter()
                .enumerate()
                .map(|(n, oid)| format!("100644 {oid} {}\tgame.rs\n", n + 1))
                .collect();
            std::fs::write(root.join("info"), info).unwrap();
            let out = super::git_command(root)
                .args(["update-index", "--index-info"])
                .stdin(std::fs::File::open(root.join("info")).unwrap())
                .output()
                .unwrap();
            std::fs::remove_file(root.join("info")).unwrap();
            assert!(out.status.success());
        };
        std::fs::write(root.join("game.rs"), b"fn game() {}\n").unwrap();

        for stage in 0..3 {
            let mut stages = [clean.as_str(); 3];
            stages[stage] = &game;
            conflict(stages);
            assert_eq!(names(root, false), ["game.rs"]);
            assert!(super::run(root, false).is_err(), "stage {}", stage + 1);

            // Gone from disk, the other stages are read as well.
            std::fs::rename(root.join("game.rs"), root.join("moved")).unwrap();
            assert!(
                super::run(root, false).is_err(),
                "stage {}, deleted",
                stage + 1
            );
            std::fs::rename(root.join("moved"), root.join("game.rs")).unwrap();
        }

        conflict([clean.as_str(); 3]);
        super::run(root, false).unwrap();
    }

    /// A submodule pulls in another repository's content, which no check
    /// here reads; it is refused whether staged or merely tracked.
    #[cfg(unix)]
    #[test]
    fn submodules_are_violations() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let oid = "0123456789abcdef0123456789abcdef01234567";
        git_str(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{oid},vendor"),
            ],
        );

        assert_eq!(names(root, true), ["vendor"]);
        assert!(super::run(root, true).is_err());
        // Not checked out, so nothing is on disk.
        assert_eq!(names(root, false), ["vendor"]);
        assert!(super::run(root, false).is_err());
    }

    /// An untracked repository inside the checkout is listed as one
    /// directory; it is refused as what it is, not as a tracked directory.
    #[cfg(unix)]
    #[test]
    fn nested_repositories_are_violations() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::fs::create_dir(root.join("inner")).unwrap();
        git_str(&root.join("inner"), &["init", "-q"]);
        std::fs::write(root.join("inner/a.rs"), b"fn a() {}\n").unwrap();

        assert_eq!(names(root, false), ["inner/"]);
        let files = super::list_files(root, false).unwrap();
        let problem = super::read(root, &files[0]).unwrap_err();
        assert!(problem.contains("nested"), "{problem}");
        assert!(super::run(root, false).is_err());
    }

    /// Git's own settings can hide a submodule from `git diff`; the staged
    /// check must see it anyway.
    #[cfg(unix)]
    #[test]
    fn staged_submodules_are_seen_whatever_git_ignores() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let oid = "0123456789abcdef0123456789abcdef01234567";
        git_str(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{oid},vendor"),
            ],
        );
        git_str(root, &["config", "diff.ignoreSubmodules", "all"]);
        // Untracked, so it is not itself staged, yet `git diff` honours it.
        std::fs::write(
            root.join(".gitmodules"),
            "[submodule \"v\"]\n\tpath = vendor\n\turl = x\n\tignore = all\n",
        )
        .unwrap();
        let hidden = git_str(root, &["diff", "--cached", "--name-only"]);
        assert!(hidden.is_empty(), "git itself hides the submodule");

        assert_eq!(names(root, true), ["vendor"]);
        assert!(super::run(root, true).is_err());
    }

    /// Tests run git in temporary repositories; from a hook (`git commit -a`
    /// sets an absolute GIT_INDEX_FILE) every one of them used to write into
    /// that one outside index and fail.
    #[test]
    fn test_git_ignores_the_repository_environment() {
        let command = super::git_command(std::path::Path::new("."));
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_str().unwrap().to_owned())
            .collect();
        for var in super::REPOSITORY_ENV {
            assert!(removed.iter().any(|r| r == var), "{var} is inherited");
        }
        // The list is git's own; a git that adds a variable fails here.
        let mut git = std::process::Command::new("git");
        super::unredirect_output(&mut git, std::env::vars_os().map(|(key, _)| key));
        let Some(out) = git
            .args(["rev-parse", "--local-env-vars"])
            .output()
            .ok()
            .filter(|o| o.status.success())
        else {
            eprintln!("skipped: no git");
            return;
        };
        for var in String::from_utf8(out.stdout).unwrap().lines() {
            assert!(super::REPOSITORY_ENV.contains(&var), "{var} is missing");
        }
    }

    /// A developer's global `core.excludesFile` used to hide the files the
    /// temporary repositories stage (`git add` refused them), and their
    /// `core.hooksPath` ran their hooks there. Only the repository's own
    /// configuration may reach those tests; the checkout keeps the rest.
    #[test]
    fn temporary_repositories_read_only_their_own_config() {
        let tmp = tempdir();
        let root = tmp.path();
        let out = super::git_command(root).args(["init", "-q"]).output();
        let Some(_) = out.ok().filter(|o| o.status.success()) else {
            eprintln!("skipped: no git");
            return;
        };
        let out = super::git_command(root)
            .args(["config", "--list", "--show-scope"])
            .output()
            .unwrap();
        assert!(out.status.success());
        for line in String::from_utf8(out.stdout).unwrap().lines() {
            assert!(
                line.starts_with("local\t") || line == "command\tsafe.directory=*",
                "outside config: {line}"
            );
        }

        let checkout = super::git_command(&crate::workspace_root());
        assert!(
            checkout
                .get_envs()
                .all(|(key, _)| key != "GIT_CONFIG_GLOBAL"),
            "the checkout's own check must keep the developer's excludes"
        );
    }

    /// Git reads `~/.config/git/ignore` and `~/.config/git/attributes` with
    /// no configuration naming them, so emptying the global config left them
    /// in force: a developer's `*.rs` there made `git add` refuse every file
    /// the temporary repositories stage, and a `filter=` would rewrite them.
    #[cfg(unix)]
    #[test]
    fn temporary_repositories_read_no_default_ignore_or_attributes() {
        let home = tempdir();
        let config = home.path().join(".config/git");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("ignore"), "*.rs\n").unwrap();
        std::fs::write(config.join("attributes"), "*.rs filter=lfs\n").unwrap();
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let probe = |command: &mut std::process::Command, args: &[&str]| {
            let out = command
                .args(args)
                .env("HOME", home.path())
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };

        // Plain git in the same place, as the tests ran before: the files
        // under that HOME apply.
        let mut plain = std::process::Command::new("git");
        plain.current_dir(root).env_remove("XDG_CONFIG_HOME");
        super::unredirect_output(&mut plain, std::env::vars_os().map(|(key, _)| key));
        for var in super::REPOSITORY_ENV.iter().chain(super::PATHSPEC_ENV) {
            plain.env_remove(var);
        }
        plain
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "safe.directory=*"]);
        assert_eq!(probe(&mut plain, &["check-ignore", "a.rs"]), "a.rs\n");

        let ignored = probe(&mut super::git_command(root), &["check-ignore", "a.rs"]);
        assert_eq!(ignored, "", "the default ignore file is read");
        let filter = probe(
            &mut super::git_command(root),
            &["check-attr", "filter", "--", "a.rs"],
        );
        assert_eq!(filter, "a.rs: filter: unspecified\n");

        // The checkout keeps the developer's own files.
        let checkout = super::git_command(&crate::workspace_root());
        assert!(
            checkout
                .get_envs()
                .all(|(key, _)| key != "XDG_CONFIG_HOME" && key != "GIT_ATTR_NOSYSTEM"),
        );
    }

    /// `GIT_TEMPLATE_DIR` and `GIT_DEFAULT_HASH` reach `git init` from the
    /// environment: a template's `info/exclude` hid the files the temporary
    /// repositories stage, its hooks ran in them (`--no-verify` skips only
    /// some), and a SHA-256 repository refused the SHA-1 ids tests stage.
    #[cfg(unix)]
    #[test]
    fn temporary_repositories_are_made_without_template_or_hash_choice() {
        let template = tempdir();
        std::fs::create_dir_all(template.path().join("info")).unwrap();
        std::fs::write(template.path().join("info/exclude"), "*.rs\n").unwrap();
        let tmp = tempdir();
        let root = tmp.path();
        let made = super::git_command(root)
            .args(["init", "-q"])
            .env("GIT_TEMPLATE_DIR", template.path())
            .env("GIT_DEFAULT_HASH", "sha256")
            .output()
            .unwrap();
        assert!(made.status.success());
        // Handed both choices, as the environment used to: both apply.
        let format = git_str(root, &["rev-parse", "--show-object-format"]);
        assert_eq!(format, b"sha256\n");
        assert!(root.join(".git/info/exclude").exists());

        let command = super::git_command(root);
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&("GIT_TEMPLATE_DIR".as_ref(), Some("".as_ref()))));
        assert!(envs.contains(&("GIT_DEFAULT_HASH".as_ref(), None)));

        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let format = git_str(root, &["rev-parse", "--show-object-format"]);
        assert_eq!(format, b"sha1\n");
        assert!(!root.join(".git/info/exclude").exists());
        assert!(!root.join(".git/hooks").exists());

        // The checkout is never initialised here and keeps its environment.
        let checkout = super::git_command(&crate::workspace_root());
        assert!(
            checkout
                .get_envs()
                .all(|(key, _)| key != "GIT_TEMPLATE_DIR" && key != "GIT_DEFAULT_HASH"),
        );
    }

    /// Under `GIT_ATTR_SOURCE=HEAD` every git command that reads attributes
    /// (`add`, `check-attr`) failed in a fresh temporary repository; under a
    /// `GIT_AUTHOR_DATE` git cannot parse, or an empty `GIT_AUTHOR_NAME`,
    /// every test that commits did; and under `GIT_LITERAL_PATHSPECS`
    /// `check-ignore` refused to run.
    #[cfg(unix)]
    #[test]
    fn temporary_repositories_ignore_attr_source_identity_and_pathspec_env() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);

        // Handed those variables, as the environment used to: git fails.
        let refused = |var: &str, value: &str, args: &[&str]| {
            let out = super::git_command(root)
                .env(var, value)
                .args(args)
                .output()
                .unwrap();
            assert!(!out.status.success(), "{var}={value:?} git {args:?}");
        };
        refused(
            "GIT_ATTR_SOURCE",
            "HEAD",
            &["check-attr", "-a", "--", "a.rs"],
        );
        refused(
            "GIT_LITERAL_PATHSPECS",
            "1",
            &["check-ignore", "-q", "--", "a.rs"],
        );
        let empty_commit = [
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c",
        ];
        let mut commit = super::git_command(root);
        commit.envs(IDENTITY).env("GIT_AUTHOR_DATE", "garbage");
        assert!(!commit.args(empty_commit).status().unwrap().success());

        let command = super::git_command(root);
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_str().unwrap().to_owned())
            .collect();
        for var in super::PATHSPEC_ENV.iter().chain(&["GIT_ATTR_SOURCE"]) {
            assert!(removed.iter().any(|r| r == var), "{var} is inherited");
        }
        // The helper's identity is its own, whatever the environment holds.
        git_str(root, &["commit", "-q", "--allow-empty", "-m", "c"]);
        let author = git_str(root, &["log", "-1", "--format=%an <%ae> %at|%cn <%ce> %ct"]);
        assert_eq!(author, b"t <t@t> 0|t <t@t> 0\n");

        // Pathspec variables change no full check (it passes none); the
        // attribute source is the checkout's own business.
        let checkout = super::git_command(&crate::workspace_root());
        assert!(checkout.get_envs().all(|(key, _)| key != "GIT_ATTR_SOURCE"));
    }

    /// A `pre-receive` hook's `GIT_QUARANTINE_PATH` made every temporary
    /// repository's `git init` fail ("ref updates forbidden inside
    /// quarantine environment"), and a TMPDIR owned by another user made
    /// git refuse each one as a dubious repository, with no global config
    /// to hold the `safe.directory` that would let it in. Git's own
    /// `GIT_TEST_ASSUME_DIFFERENT_OWNER` stands in for that owner.
    #[cfg(unix)]
    #[test]
    fn temporary_repositories_ignore_quarantine_and_ownership() {
        let tmp = tempdir();
        let root = tmp.path();
        let control = tempdir();

        // Plain git, as the tests used to run it: refused.
        let init = super::git_command(control.path())
            .env("GIT_QUARANTINE_PATH", root)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(!init.status.success(), "quarantine allowed git init");
        git_str(control.path(), &["init", "-q"]);
        // As test git, less its trust (and with no config that could hold one).
        let mut plain = std::process::Command::new("git");
        for var in super::REPOSITORY_ENV {
            plain.env_remove(var);
        }
        let dubious = plain
            .current_dir(control.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .args(["rev-parse", "--git-dir"])
            .output()
            .unwrap();
        assert!(!dubious.status.success(), "ownership was not checked");

        // Test git drops the quarantine, and trusts a repository another
        // user owns, so the check runs.
        let command = super::git_command(root);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "GIT_QUARANTINE_PATH" && value.is_none()),
            "GIT_QUARANTINE_PATH is inherited"
        );
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("game.bin"), b"MZ\x90\0").unwrap();
        let add = super::git_command(root)
            .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
            .args(["add", "game.bin"])
            .output()
            .unwrap();
        assert!(add.status.success(), "ownership refused the test");
        assert_eq!(names(root, true), ["game.bin"]);

        // The checkout keeps both checks.
        let checkout = super::git_command(&crate::workspace_root());
        assert!(
            checkout
                .get_envs()
                .all(|(key, _)| key != "GIT_QUARANTINE_PATH")
        );
        assert!(checkout.get_args().all(|arg| arg != "safe.directory=*"));
    }

    /// Trace output pointed at stdout used to be parsed as git's output: the
    /// first file listed was taken for a name that does not exist and went
    /// unread, and `--staged` failed on the record. The variables are
    /// dropped (trace2's set off) for every git the check runs.
    #[cfg(unix)]
    #[test]
    fn trace_output_is_not_read_as_git_output() {
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        std::fs::write(root.join("a.bin"), b"MZ\x90\0").unwrap();
        git_str(root, &["add", "a.bin"]);
        const TRACES: [&str; 4] = [
            "GIT_TRACE",
            "GIT_TRACE_SETUP",
            "GIT_TRACE_PERFORMANCE",
            "GIT_TRACE2_EVENT",
        ];
        let ls_files = |unredirect: bool| {
            let mut command = super::git_command(root);
            for var in TRACES {
                command.env(var, "/dev/stdout");
            }
            command.env("GIT_TRACE2", "/dev/stdout");
            if unredirect {
                let keys = TRACES.iter().chain(&["GIT_TRACE2", "GIT_REDIRECT_STDOUT"]);
                super::unredirect_output(&mut command, keys.map(std::ffi::OsString::from));
            }
            command.args(["ls-files", "-z"]).output().unwrap()
        };

        // As git was run before: trace lines lead the first name.
        let traced = ls_files(false);
        assert!(traced.status.success());
        assert_ne!(traced.stdout, b"a.bin\0", "git wrote no trace to stdout");
        // As it is run now: only the names.
        let plain = ls_files(true);
        assert!(plain.status.success());
        assert_eq!(plain.stdout, b"a.bin\0");

        // And whatever this process inherited, the check reads the file.
        assert_eq!(names(root, false), ["a.bin"]);
        assert!(super::run(root, false).is_err());
        assert!(super::run(root, true).is_err());
    }

    /// A temporary directory that is not a repository is walked. Made beside
    /// the test binary, it used to be taken for an ignored part of the
    /// checkout: `git ls-files` there succeeded, listed nothing, and the
    /// check passed a directory it never read.
    #[cfg(unix)]
    #[test]
    fn directories_outside_any_repository_are_walked() {
        let outer = tempdir();
        git_str(outer.path(), &["init", "-q"]);
        std::fs::write(outer.path().join(".gitignore"), "plain/\n").unwrap();
        let root = outer.path().join("plain");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(root.join("target/game.bin"), b"MZ\x90\0build output").unwrap();
        std::fs::write(root.join("game.bin"), b"MZ\x90\0a game executable").unwrap();

        // Without the ceiling, as before: git finds the enclosing repository,
        // where the directory is ignored and lists as empty.
        let found = super::git_command(&root)
            .env_remove("GIT_CEILING_DIRECTORIES")
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .output()
            .unwrap();
        assert!(found.status.success());
        assert!(found.stdout.is_empty());

        let outside = super::git_command(&root)
            .args(["rev-parse", "--git-dir"])
            .output()
            .unwrap();
        assert!(!outside.status.success(), "a repository above was found");
        // The walk skips build output and lists in no fixed order.
        let mut walked = names(&root, false);
        walked.sort();
        assert_eq!(walked, ["game.bin", "src/a.rs"]);
        assert!(super::run(&root, false).is_err());
        std::fs::remove_file(root.join("game.bin")).unwrap();
        super::run(&root, false).unwrap();

        // The checkout keeps git's own discovery.
        let checkout = super::git_command(&crate::workspace_root());
        assert!(
            checkout
                .get_envs()
                .all(|(key, _)| key != "GIT_CEILING_DIRECTORIES")
        );
    }

    /// Outside a repository only regular files were walked: a symlink or a
    /// FIFO passed unseen, though git lists both, and a regular file or
    /// symlink named `target` was passed over like the build directory. A
    /// tree unpacked from a source archive, or checked out to a directory
    /// named `target`, was not checked at all.
    #[cfg(unix)]
    #[test]
    fn directories_outside_any_repository_list_every_entry_git_would() {
        let dir = tempdir();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("nested/target")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"fn a() {}\n").unwrap();
        std::fs::write(root.join("nested/target/game.bin"), b"MZ\x90\0build output").unwrap();
        std::fs::write(root.join("elsewhere/lib.rs"), b"// kept\n").unwrap();
        // Read as git stores it: the target path, under a name not allowed.
        std::os::unix::fs::symlink("src/a.rs", root.join("game.xex")).unwrap();
        // A regular file named after the build directory is still a file.
        std::fs::write(root.join("src/target"), b"MZ\x90\0a game executable").unwrap();
        // A symlink that leads to a directory is passed over like one.
        std::os::unix::fs::symlink("elsewhere", root.join("target")).unwrap();
        // `.git` of any kind is git's own.
        std::fs::write(root.join("nested/.git"), b"gitdir: /nowhere\n").unwrap();
        let fifo = std::process::Command::new("mkfifo")
            .arg(root.join("pipe.rs"))
            .status()
            .unwrap();
        assert!(fifo.success());

        let mut walked = names(&root, false);
        walked.sort();
        assert_eq!(
            walked,
            [
                "elsewhere/lib.rs",
                "game.xex",
                "pipe.rs",
                "src/a.rs",
                "src/target"
            ]
        );
        // Not blocked on the FIFO, and each of the three is a violation.
        let files = super::list_files(&root, false).unwrap();
        let problems: Vec<_> = files
            .iter()
            .map(|file| (file.name.as_str(), super::read(&root, file)))
            .collect();
        for (name, read) in &problems {
            match *name {
                "pipe.rs" => assert_eq!(read.as_ref().unwrap_err(), "not a regular file"),
                "game.xex" => {
                    assert_eq!(read.as_ref().unwrap().as_ref().unwrap().head, b"src/a.rs")
                }
                _ => assert!(read.is_ok(), "{name}: {read:?}"),
            }
        }
        assert!(super::run(&root, false).is_err());
        for name in ["game.xex", "src/target", "pipe.rs"] {
            std::fs::remove_file(root.join(name)).unwrap();
        }
        super::run(&root, false).unwrap();

        // A tree whose own top is named `target` is walked, not skipped.
        let top = dir.path().join("target");
        std::fs::create_dir_all(&top).unwrap();
        std::fs::write(top.join("game.bin"), b"MZ\x90\0a game executable").unwrap();
        assert_eq!(names(&top, false), ["game.bin"]);
        assert!(super::run(&top, false).is_err());
    }

    /// Under a TMPDIR holding `:`, the ceiling put on a temporary directory
    /// was split there and did not hold: git found the repository around it,
    /// and the directory was taken for part of that repository instead of
    /// being walked. Under one holding `'`, the hook and filter scripts the
    /// tests write were shell syntax errors. Neither is made any more.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_survive_any_tmpdir_name() {
        let outer = tempdir();
        git_str(outer.path(), &["init", "-q"]);
        let colon = outer.path().join("a:b");
        let plain = outer.path().join("plain");
        std::fs::create_dir(&colon).unwrap();
        std::fs::create_dir(&plain).unwrap();

        // As before: inside `a:b`, the ceiling is split and git looks above.
        let inner = colon.join("tmp");
        std::fs::create_dir(&inner).unwrap();
        let found = super::git_command(&inner)
            .args(["rev-parse", "--git-dir"])
            .output()
            .unwrap();
        assert!(found.status.success(), "the ceiling held after all");

        let tmp = tempdir_in_first(&crate::workspace_root(), [colon, plain.clone()]);
        assert!(tmp.path().starts_with(plain.canonicalize().unwrap()));
        let found = super::git_command(tmp.path())
            .args(["rev-parse", "--git-dir"])
            .output()
            .unwrap();
        assert!(!found.status.success(), "a repository above was found");

        let tricky = outer.path().join("it's a $dir:`x` \\ \"q\"");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", sh_quote(&tricky)))
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, tricky.as_os_str().as_encoded_bytes());
    }

    /// The checkout reached by another spelling (a symlink to it, or a
    /// path through `..`) is still the checkout: git reads its global and
    /// system configuration there, as the full check does, and only
    /// temporary repositories are cut off from it.
    #[cfg(unix)]
    #[test]
    fn the_checkout_keeps_its_configuration_however_it_is_reached() {
        let isolated = |root: &std::path::Path| {
            super::git_command(root)
                .get_envs()
                .any(|(key, _)| key == "GIT_CONFIG_GLOBAL")
        };
        let checkout = crate::workspace_root();
        let links = tempdir();
        let link = links.path().join("checkout");
        std::os::unix::fs::symlink(&checkout, &link).unwrap();
        let dotted = checkout.join("xtask").join("..");
        for root in [
            checkout.clone(),
            checkout.canonicalize().unwrap(),
            link,
            dotted,
        ] {
            assert!(!isolated(&root), "{} was isolated", root.display());
        }
        // Not the checkout: a temporary repository, and one beside it.
        assert!(isolated(links.path()));
        assert!(isolated(&checkout.join("xtask")));
    }

    /// Under a TMPDIR whose path is not UTF-8, the tests that write hook
    /// or filter scripts, or pass a path inside it to git as text, panicked
    /// in `to_str`. Such a directory is passed over like one holding `:`.
    #[cfg(unix)]
    #[test]
    fn temporary_directories_skip_a_tmpdir_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let outer = tempdir();
        let bad = outer
            .path()
            .join(std::ffi::OsStr::from_bytes(b"not-utf8-\xff"));
        let plain = outer.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        if std::fs::create_dir(&bad).is_err() {
            // Some filesystems (APFS) refuse names that are not UTF-8.
            return;
        }
        assert!(
            tempfile::tempdir_in(&bad)
                .unwrap()
                .path()
                .to_str()
                .is_none()
        );

        let tmp = tempdir_in_first(&crate::workspace_root(), [bad.clone(), plain.clone()]);
        assert!(tmp.path().starts_with(plain.canonicalize().unwrap()));
        assert!(tmp.path().to_str().is_some());
        sh_quote(tmp.path());
        assert_eq!(std::fs::read_dir(&bad).unwrap().count(), 0);
    }

    const LFS_POINTER: &[u8] = b"version https://git-lfs.github.com/spec/v1\n\
        oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
        size 12345\n";

    /// A Git LFS pointer is small, allowlisted text, but the file it stands
    /// for goes to LFS storage, which no check reads.
    /// `git commit` runs `pre-commit`, but a merge made without conflicts
    /// runs `pre-merge-commit` instead, and `git am` runs `pre-applypatch`:
    /// with only `pre-commit` installed, a branch committed with
    /// `--no-verify` merged its files, and a mailed patch applied its
    /// files, past the check. Every tracked hook is run here in place of the
    /// real one (which builds xtask), and each way of committing must run
    /// at least one of them; the staged check then sees what it brings in.
    #[cfg(unix)]
    #[test]
    fn every_way_of_committing_runs_a_tracked_hook() {
        use std::os::unix::fs::PermissionsExt;
        let checkout = crate::workspace_root();
        let Some(out) = super::git_command(&checkout)
            .args(["ls-files", "-z", "--", ".githooks"])
            .output()
            .ok()
            .filter(|o| o.status.success())
        else {
            eprintln!("skipped: not a git checkout");
            return;
        };
        let hooks: Vec<String> = out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| {
                String::from_utf8_lossy(s)
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();

        let tmp = tempdir();
        let root = tmp.path().join("repo");
        let hooks_dir = tmp.path().join("hooks");
        let ran = tmp.path().join("ran");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&hooks_dir).unwrap();
        for hook in &hooks {
            let path = hooks_dir.join(hook);
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho {hook} >> {}\n", sh_quote(&ran)),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let hooked = format!("core.hooksPath={}", hooks_dir.display());
        let ran_hooks = || {
            let log = std::fs::read_to_string(&ran).unwrap_or_default();
            let _ = std::fs::remove_file(&ran);
            log
        };

        git_str(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        git_str(&root, &["add", "a.rs"]);
        git_str(&root, &["-c", &hooked, "commit", "-q", "-m", "a"]);
        assert!(!ran_hooks().is_empty(), "git commit ran no tracked hook");

        git_str(&root, &["checkout", "-q", "-b", "side"]);
        std::fs::write(root.join("data.rs"), b"MZ\x90\0binary").unwrap();
        git_str(&root, &["add", "data.rs"]);
        git_str(&root, &["commit", "-q", "--no-verify", "-m", "data"]);
        let patch = git_str(&root, &["format-patch", "-1", "--stdout"]);
        git_str(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("b.rs"), b"fn b() {}\n").unwrap();
        git_str(&root, &["add", "b.rs"]);
        git_str(&root, &["commit", "-q", "--no-verify", "-m", "b"]);

        git_str(&root, &["merge", "-q", "--no-commit", "side"]);
        assert!(
            super::run(&root, true).is_err(),
            "the merged file is staged"
        );
        git_str(&root, &["merge", "--abort"]);
        git_str(
            &root,
            &["-c", &hooked, "merge", "-q", "-m", "merge", "side"],
        );
        assert!(!ran_hooks().is_empty(), "git merge ran no tracked hook");

        git_str(&root, &["reset", "-q", "--hard", "HEAD~1"]);
        let patch_file = tmp.path().join("data.patch");
        std::fs::write(&patch_file, patch).unwrap();
        let patch_file = patch_file.to_str().unwrap();
        git_str(&root, &["-c", &hooked, "am", "-q", patch_file]);
        assert!(!ran_hooks().is_empty(), "git am ran no tracked hook");
    }

    /// Git clients started from a desktop launcher run hooks with a PATH
    /// that often lacks `~/.cargo/bin`, where rustup puts cargo, and every
    /// hook failed with only `cargo: not found`. Each tracked hook is run
    /// here with an empty PATH: it must find cargo under `CARGO_HOME`, or
    /// `~/.cargo` without it, and run the staged check; with cargo nowhere it
    /// must refuse with a message naming cargo.
    #[cfg(unix)]
    #[test]
    fn hooks_find_cargo_off_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let checkout = crate::workspace_root();
        let Some(out) = super::git_command(&checkout)
            .args(["ls-files", "-z", "--", ".githooks"])
            .output()
            .ok()
            .filter(|o| o.status.success())
        else {
            eprintln!("skipped: not a git checkout");
            return;
        };
        let hooks: Vec<std::path::PathBuf> = out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| checkout.join(String::from_utf8_lossy(s).as_ref()))
            .collect();
        assert!(!hooks.is_empty(), ".githooks/ holds no tracked hooks");

        let tmp = tempdir();
        let empty_path = tmp.path().join("bin");
        let home = tmp.path().join("home");
        let bin = home.join(".cargo/bin");
        let log = tmp.path().join("ran");
        std::fs::create_dir(&empty_path).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let cargo = bin.join("cargo");
        std::fs::write(
            &cargo,
            format!("#!/bin/sh\necho \"$*\" >> {}\n", sh_quote(&log)),
        )
        .unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let nowhere = tmp.path().join("nowhere");

        let run = |hook: &std::path::Path, vars: &[(&str, &std::path::Path)]| {
            let mut command = std::process::Command::new(hook);
            command
                .current_dir(&checkout)
                .env_clear()
                .env("PATH", &empty_path);
            for (var, value) in vars {
                command.env(var, value);
            }
            let out = command.output().unwrap();
            let ran = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            (out, ran)
        };
        for hook in &hooks {
            for vars in [
                &[("HOME", home.as_path())][..],
                &[("HOME", &nowhere), ("CARGO_HOME", &home.join(".cargo"))],
            ] {
                let (out, ran) = run(hook, vars);
                assert!(
                    out.status.success(),
                    "{} with {vars:?}: {}",
                    hook.display(),
                    String::from_utf8_lossy(&out.stderr)
                );
                assert_eq!(ran, "xtask policy --staged\n", "{}", hook.display());
            }
            let (out, ran) = run(hook, &[("HOME", &nowhere)]);
            assert!(
                !out.status.success(),
                "{} ran without cargo",
                hook.display()
            );
            assert!(ran.is_empty());
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("cargo not found"),
                "{}: {}",
                hook.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // Git for Windows can run cargo.exe. A searchable directory called
        // cargo must not mask that executable, and a policy failure from it
        // must reach Git through each hook.
        std::fs::rename(&cargo, bin.join("cargo.exe")).unwrap();
        std::fs::create_dir(&cargo).unwrap();
        std::fs::write(
            bin.join("cargo.exe"),
            format!("#!/bin/sh\necho \"$*\" >> {}\nexit 37\n", sh_quote(&log)),
        )
        .unwrap();
        for hook in &hooks {
            let (out, ran) = run(hook, &[("HOME", &home)]);
            assert_eq!(ran, "xtask policy --staged\n", "{}", hook.display());
            assert_eq!(out.status.code(), Some(37), "{}", hook.display());
        }
    }

    #[test]
    fn rejects_lfs_pointers() {
        assert!(!check("assets/model.txt", LFS_POINTER).is_empty());
        let legacy = b"version https://hawser.github.com/spec/v1\noid sha256:00\nsize 1\n";
        assert!(!check("data.json", legacy).is_empty());
        assert!(
            check("notes.md", b"See https://git-lfs.github.com/spec/v1.\n").is_empty(),
            "only a pointer, not a mention"
        );
    }

    /// With `filter=lfs`, the index holds the pointer while the working tree
    /// holds the real file: the staged check reads only the pointer, and a
    /// checkout without git-lfs sees only the pointer too. Both checks must
    /// refuse it. The clean filter here stands in for git-lfs, which the
    /// test does not need installed.
    #[cfg(unix)]
    #[test]
    fn lfs_tracked_files_are_violations() {
        let outside = tempdir();
        let pointer = outside.path().join("pointer");
        std::fs::write(&pointer, LFS_POINTER).unwrap();
        let tmp = tempdir();
        let root = tmp.path();
        git_str(root, &["init", "-q"]);
        let clean = format!("cat {}", sh_quote(&pointer));
        git_str(root, &["config", "filter.lfs.clean", &clean]);
        git_str(root, &["config", "filter.lfs.smudge", "cat"]);
        git_str(root, &["config", "filter.lfs.required", "true"]);
        std::fs::write(root.join(".gitattributes"), "*.txt filter=lfs -text\n").unwrap();
        std::fs::write(root.join("strings.txt"), "plain text on disk\n").unwrap();
        git_str(root, &["add", ".gitattributes", "strings.txt"]);
        let staged = git_str(root, &["cat-file", "blob", ":strings.txt"]);
        assert_eq!(staged, LFS_POINTER, "the index holds the pointer");
        assert!(super::run(root, true).is_err());

        // A checkout without the filter: the pointer is the file on disk.
        std::fs::write(root.join("strings.txt"), LFS_POINTER).unwrap();
        assert!(super::run(root, false).is_err());
    }

    #[test]
    fn rejects_data_smuggled_into_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!check("src/blob.rs", blob.as_bytes()).is_empty());
    }

    /// `base64` output wrapped at 76 columns, PEM-style 64-column lines and
    /// `xxd -p` hex at 60, with LF or CRLF: each line is short, the run is not.
    #[test]
    fn rejects_encoded_data_wrapped_across_lines() {
        for (width, alphabet, eol) in [
            (
                76,
                &b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"[..],
                "\n",
            ),
            (
                64,
                &b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"[..],
                "\r\n",
            ),
            (60, &b"0123456789abcdef"[..], "\n"),
        ] {
            let line: String = (0..width)
                .map(|i| alphabet[i * 7 % alphabet.len()] as char)
                .collect();
            let lines = MAX_ENCODED_RUN / width + 1;
            let blob = format!("{line}{eol}").repeat(lines) + "QUJD==" + eol;
            assert_eq!(
                super::longest_encoded_run(blob.as_bytes()),
                lines * width + 6,
                "width {width}, eol {eol:?}"
            );
            assert!(
                check("strings.txt", blob.as_bytes())
                    .iter()
                    .any(|p| p.contains("hex/base64 run")),
                "width {width}, eol {eol:?}"
            );
        }
    }

    /// base64url (`-` and `_` for `+` and `/`) is as much a payload as base64,
    /// wrapped or not; the two alphabets are not mixed into one run.
    #[test]
    fn rejects_base64url_data() {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let encoded: String = (0..MAX_ENCODED_RUN * 2)
            .map(|i| alphabet[i * 7 % alphabet.len()] as char)
            .collect();
        assert!(
            encoded.contains('-') && encoded.contains('_'),
            "the test data uses the URL-safe characters"
        );
        assert!(
            super::longest_run(encoded.as_bytes(), b"+/=") < 64,
            "the standard alphabet alone sees only short runs"
        );
        for blob in [
            format!("const TOKEN: &str = \"{encoded}\";\n"),
            encoded
                .as_bytes()
                .chunks(76)
                .map(|line| String::from_utf8_lossy(line) + "\n")
                .collect(),
        ] {
            assert!(
                check("strings.txt", blob.as_bytes())
                    .iter()
                    .any(|p| p.contains("hex/base64 run")),
                "{blob:?}"
            );
        }
        let run = super::longest_encoded_run;
        assert_eq!(run(b"ab-cd_ef"), 8);
        assert_eq!(run(b"ab+cd/ef"), 8);
        assert_eq!(run(b"ab+cd-ef"), 5, "the alphabets do not mix");
        assert_eq!(run(b"ab-cd\nef_gh"), 10, "wrapped like base64");
    }

    /// Only a single line break joins: prose, code, a blank line or
    /// indentation between payload characters still ends the run.
    #[test]
    fn line_breaks_join_only_payload_lines() {
        let run = super::longest_encoded_run;
        assert_eq!(run(b"ab\ncd"), 4);
        assert_eq!(run(b"ab\r\ncd"), 4);
        assert_eq!(run(b"ab\n\ncd"), 2, "a blank line ends the run");
        assert_eq!(run(b"ab\n cd"), 2, "indentation ends the run");
        assert_eq!(run(b"ab\r\rcd"), 2);
        assert_eq!(
            run(b"\nab\n"),
            2,
            "leading and trailing breaks count nothing"
        );
        assert_eq!(run(b""), 0);
        let paragraph = "The quick brown fox\njumps over the lazy dog\n".repeat(200);
        assert!(run(paragraph.as_bytes()) < 20);
    }

    /// ascii85, Z85 and uuencode alphabets, unwrapped and wrapped: punctuation
    /// breaks every hex/base64 run, the data is there all the same.
    #[test]
    fn rejects_ascii85_z85_and_uuencoded_data() {
        let ascii85: Vec<u8> = (b'!'..=b'u').collect();
        let uuencode: Vec<u8> = (b'!'..=b'`').collect();
        let z85 = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-:+=^!/*?&<>()[]{}@%$#";
        for (what, alphabet, width) in [
            ("ascii85", &ascii85[..], 76),
            ("Z85", &z85[..], 76),
            ("uuencode", &uuencode[..], 61),
        ] {
            let encoded: String = (0..MAX_ENCODED_RUN * 2)
                .map(|i| alphabet[i * 7 % alphabet.len()] as char)
                .collect();
            assert!(
                super::longest_encoded_run(encoded.as_bytes()) < 32,
                "{what}: the hex/base64 check alone sees only short runs"
            );
            let wrapped: String = encoded
                .as_bytes()
                .chunks(width)
                .map(|line| String::from_utf8_lossy(line) + "\n")
                .collect();
            for blob in [
                encoded.clone(),
                wrapped.clone(),
                wrapped.replace('\n', "\r\n"),
            ] {
                assert_eq!(
                    super::longest_printable_run(blob.as_bytes()),
                    encoded.len(),
                    "{what}"
                );
                assert!(
                    check("strings.txt", blob.as_bytes())
                        .iter()
                        .any(|p| p.contains("without whitespace")),
                    "{what}: {blob:?}"
                );
            }
        }
    }

    /// Whitespace ends a printable run, and a line break continues one only
    /// after a line as long as an encoder writes.
    #[test]
    fn printable_runs_join_only_long_lines() {
        let run = super::longest_printable_run;
        assert_eq!(run(b"a+b c"), 3);
        assert_eq!(run(b"ab\n\ncd"), 2);
        let long = "x!".repeat(super::MIN_WRAPPED_LINE / 2);
        assert_eq!(run(format!("{long}\n{long}").as_bytes()), long.len() * 2);
        assert_eq!(run(format!("{long}\n {long}").as_bytes()), long.len());
        assert_eq!(run(format!("x!\n{long}").as_bytes()), long.len());
        let gitignore = "/target/\n*.exe\n*.dll\n**/*.bin\n".repeat(100);
        assert!(check(".gitignore", gitignore.as_bytes()).is_empty());
        let url = format!("<https://example.com/{}?a=1&b=2>\n", "p/".repeat(150));
        assert!(check("README.md", url.as_bytes()).is_empty());
    }

    /// Binary data with every NUL removed, by COBS byte stuffing here, is
    /// still binary: it is not UTF-8, or holds control characters.
    #[test]
    fn rejects_binary_without_nul_bytes() {
        fn cobs(data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            for block in data.split(|&b| b == 0) {
                for chunk in block.chunks(254) {
                    out.push(chunk.len() as u8 + 1);
                    out.extend_from_slice(chunk);
                }
                if block.is_empty() {
                    out.push(1);
                }
            }
            out
        }
        let mut state = 0x9e37_79b9u32;
        let bytes: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();
        let stuffed = cobs(&bytes);
        assert!(!stuffed.contains(&0));
        assert!(super::longest_encoded_run(&stuffed) <= MAX_ENCODED_RUN);
        assert!(super::longest_printable_run(&stuffed) <= MAX_ENCODED_RUN);
        let problems = check("strings.txt", &stuffed);
        assert!(
            problems.iter().any(|p| p.contains("not UTF-8")),
            "{problems:?}"
        );
        // Bytes under 0x80 only (7-bit stuffing) are UTF-8, but not text.
        let seven: Vec<u8> = stuffed.iter().map(|b| (b & 0x7f).max(1)).collect();
        let problems = check("strings.txt", &seven);
        assert!(
            problems.iter().any(|p| p.contains("control character")),
            "{problems:?}"
        );
        for ok in [&b"a\tb\r\n\x0cc\n"[..], "naïve café — → ✓\n".as_bytes()] {
            assert!(check("README.md", ok).is_empty(), "{ok:?}");
        }
    }

    /// base32768 and base65536 write 15 or 16 bits per letter, CJK and the
    /// like, without whitespace: a run of them is a printable run too.
    #[test]
    fn rejects_base65536_like_data() {
        let encoded: String = (0..3000u32)
            .map(|i| char::from_u32(0x4e00 + (i * 7919) % 0x5000).unwrap())
            .collect();
        assert_eq!(super::longest_encoded_run(encoded.as_bytes()), 0);
        for blob in [
            format!("const DATA: &str = \"{encoded}\";\n"),
            encoded
                .chars()
                .collect::<Vec<_>>()
                .chunks(64)
                .map(|line| line.iter().collect::<String>() + "\n")
                .collect(),
        ] {
            assert!(
                check("src/data.rs", blob.as_bytes())
                    .iter()
                    .any(|p| p.contains("without whitespace")),
                "{blob:.80}"
            );
        }
        // Prose in any script, with its spaces and line breaks, passes.
        let prose = "日本語の文章。短い行です。\n".repeat(200);
        assert!(check("docs/notes.md", prose.as_bytes()).is_empty());
    }

    /// `xxd -i` output, a Rust byte array and a byte-escape string: every
    /// token is short, but the file is an executable all the same.
    #[test]
    fn rejects_data_as_numeric_arrays_and_escapes() {
        let bytes: Vec<u8> = (0..MAX_LITERAL_RUN * 2).map(|i| (i * 37) as u8).collect();
        let xxd: String = bytes
            .chunks(12)
            .map(|row| {
                let row: Vec<String> = row.iter().map(|b| format!("0x{b:02x}")).collect();
                format!("  {},\n", row.join(", "))
            })
            .collect();
        let decimal: Vec<String> = bytes.iter().map(|b| format!("{b}u8")).collect();
        let escaped: String = bytes.iter().map(|b| format!("\\x{b:02x}")).collect();
        for blob in [
            format!("unsigned char data[] = {{\n{xxd}}};\n"),
            format!(
                "const DATA: [u8; {}] = [{}];\n",
                bytes.len(),
                decimal.join(",")
            ),
            format!("const DATA: &[u8] = b\"{escaped}\";\n"),
        ] {
            assert_eq!(
                super::longest_literal_run(blob.as_bytes()),
                bytes.len(),
                "{blob:.80}"
            );
            assert!(
                check("src/data.rs", blob.as_bytes())
                    .iter()
                    .any(|p| p.contains("numeric array")),
                "{blob:.80}"
            );
        }
    }

    /// The same escapes split into many short strings, each well under the
    /// limit: a Rust string continuation, C/HLSL adjacent literals,
    /// `concat!` arguments, an array of byte strings, and `\u{..}` escapes.
    #[test]
    fn rejects_escape_strings_split_across_literals() {
        let bytes: Vec<u8> = (0..MAX_LITERAL_RUN * 2).map(|i| (i * 37) as u8).collect();
        let rows = |escape: &dyn Fn(u8) -> String, join: &str| -> String {
            bytes
                .chunks(64)
                .map(|row| row.iter().map(|&b| escape(b)).collect::<String>())
                .collect::<Vec<_>>()
                .join(join)
        };
        let x = |b: u8| format!("\\x{b:02x}");
        let u = |b: u8| format!("\\u{{{b:x}}}");
        for blob in [
            format!("const DATA: &[u8] = b\"{}\";\n", rows(&x, "\\\n    ")),
            format!("const DATA: &[u8] = b\"{}\";\n", rows(&x, "\\\r\n\t")),
            format!("const char data[] =\n    \"{}\";\n", rows(&x, "\"\n    \"")),
            format!("const DATA: &str = concat!(\"{}\");\n", rows(&x, "\", \"")),
            format!(
                "const DATA: &[&[u8]] = &[\n    b\"{}\",\n];\n",
                rows(&x, "\",\n    b\"")
            ),
            format!("const DATA: &str = \"{}\";\n", rows(&u, "\\\n    ")),
        ] {
            assert_eq!(
                super::longest_literal_run(blob.as_bytes()),
                bytes.len(),
                "{blob:.80}"
            );
            assert!(
                check("src/data.rs", blob.as_bytes())
                    .iter()
                    .any(|p| p.contains("byte-escape string")),
                "{blob:.80}"
            );
        }
    }

    /// Octal and fixed-width Unicode escapes, which C, Python, shell
    /// `printf` and JSON write, split into short strings as above: each
    /// string is a short printable run, and none was a `\xHH` escape.
    #[test]
    fn rejects_octal_and_fixed_width_unicode_escapes() {
        let bytes: Vec<u8> = (0..MAX_LITERAL_RUN * 2).map(|i| (i * 37) as u8).collect();
        let rows = |escape: &dyn Fn(u8) -> String, join: &str| -> String {
            bytes
                .chunks(40)
                .map(|row| row.iter().map(|&b| escape(b)).collect::<String>())
                .collect::<Vec<_>>()
                .join(join)
        };
        let octal = |b: u8| format!("\\{b:o}");
        let octal3 = |b: u8| format!("\\{b:03o}");
        let u4 = |b: u8| format!("\\u{:04x}", u16::from(b) + 0x80);
        let u8_ = |b: u8| format!("\\U{:08X}", u32::from(b) + 0x10000);
        for (path, blob) in [
            (
                "src/data.h",
                format!(
                    "static const char data[] =\n    \"{}\";\n",
                    rows(&octal, "\"\n    \"")
                ),
            ),
            (
                "tools/data.py",
                format!("DATA = (\n    b\"{}\"\n)\n", rows(&octal3, "\"\n    b\"")),
            ),
            (
                "data/strings.json",
                format!("[\n  \"{}\"\n]\n", rows(&u4, "\",\n  \"")),
            ),
            (
                "tools/wide.py",
                format!("DATA = \"{}\"\n", rows(&u8_, "\" \"")),
            ),
        ] {
            assert!(
                super::longest_printable_run(blob.as_bytes()) <= MAX_ENCODED_RUN,
                "{path}: each string is short"
            );
            assert_eq!(
                super::longest_literal_run(blob.as_bytes()),
                bytes.len(),
                "{path}: {blob:.80}"
            );
            assert!(
                check(path, blob.as_bytes())
                    .iter()
                    .any(|p| p.contains("byte-escape string")),
                "{path}: {blob:.80}"
            );
        }

        let run = super::longest_literal_run;
        assert_eq!(run(br#""\0""#), 1, "a NUL terminator is one escape");
        assert_eq!(run(br#""\1234""#), 1, "at most three octal digits");
        assert_eq!(run(br#""\u00e9\u00""#), 1, "\\u takes exactly four digits");
        assert_eq!(run(br#""\U0001F600\U1F600""#), 1, "\\U takes exactly eight");
        assert_eq!(run(br#""caf\u00e9", "na\u00efve""#), 1);
        assert_eq!(
            run(br#""\u{1F600}\u00e9\101""#),
            3,
            "escapes of all kinds join"
        );
    }

    /// Strings of escapes join only when nothing but a string boundary lies
    /// between them.
    #[test]
    fn escape_runs_join_only_across_string_boundaries() {
        let run = super::longest_literal_run;
        assert_eq!(
            run(br#""\x1b[0m" "\x1b[1m""#),
            1,
            "text between escapes ends a run"
        );
        assert_eq!(run(br#"f("\x00", 1, "\x01")"#), 1, "an argument ends a run");
        assert_eq!(run(br#""\x00\x01", // two"#), 2);
        assert_eq!(run(br#""\x00\x01", "\x02""#), 3);
        assert_eq!(run(b"\"\\x00\\\n   \\x01\""), 2);
        assert_eq!(
            run(br"\x00, \x01"),
            1,
            "a comma alone is text inside one string"
        );
        assert_eq!(
            run(br#""\u{1F600}\u{1}\u{}\u{1234567}""#),
            2,
            "one to six digits"
        );
        assert_eq!(run(br#""caf\u{e9}", "na\u{ef}ve""#), 1);
    }

    /// Tables split into rows, numbers inside identifiers, prose and the odd
    /// escape stay short.
    #[test]
    fn literal_runs_end_at_anything_but_commas() {
        let run = super::longest_literal_run;
        assert_eq!(run(b"[1, 2,\n 3,\r\n\t-4, 0x5_u8, 6.5]"), 6);
        assert_eq!(run(b"[[1, 2], [3, 4]]"), 2, "rows count one by one");
        assert_eq!(run(b"1, 2, // three\n4, 5"), 2, "a comment ends the list");
        assert_eq!(
            run(b"f(x86_64, u8, 2)"),
            1,
            "digits inside identifiers are not literals"
        );
        assert_eq!(run(b"a - 1, b"), 1);
        assert_eq!(run(br"\x4d\x5a\x90\x00 \x01"), 4);
        assert_eq!(run(br"\x4\xzz\\x"), 0);
        assert_eq!(run(b""), 0);
        let table: String = (0..64)
            .map(|r| {
                format!(
                    "    [{}],\n",
                    (0..16)
                        .map(|c| (r * c % 7).to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect();
        assert_eq!(run(table.as_bytes()), 16);
        assert!(check("src/table.rs", table.as_bytes()).is_empty());
    }

    /// A dump of binary data as `xxd`, `hexdump -C`, `od` and space-separated
    /// `0x` bytes write it: short groups and whitespace, no commas. Each
    /// passed every other check.
    #[test]
    fn rejects_hex_dumps() {
        let bytes: Vec<u8> = (0..MAX_LITERAL_RUN * 4).map(|i| (i * 37) as u8).collect();
        let printable = |row: &[u8]| -> String {
            row.iter()
                .map(|&b| if b.is_ascii_graphic() { b as char } else { '.' })
                .collect()
        };
        let lines = |width: usize, line: &dyn Fn(usize, &[u8]) -> String| -> String {
            bytes
                .chunks(width)
                .enumerate()
                .map(|(i, row)| line(i * width, row) + "\n")
                .collect()
        };
        let xxd = lines(16, &|at, row| {
            let groups: Vec<String> = row
                .chunks(2)
                .map(|g| g.iter().map(|b| format!("{b:02x}")).collect())
                .collect();
            format!("{at:08x}: {}  {}", groups.join(" "), printable(row))
        });
        let hexdump = lines(16, &|at, row| {
            let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
            format!(
                "{at:08x}  {}  {}  |{}|",
                hex[..8].join(" "),
                hex[8..].join(" "),
                printable(row)
            )
        });
        let od = lines(16, &|at, row| {
            let words: Vec<String> = row
                .chunks(2)
                .map(|w| format!("{:06o}", u16::from_le_bytes([w[0], w[1]])))
                .collect();
            format!("{at:07o} {}", words.join(" "))
        });
        let spaced = lines(12, &|_, row| {
            let hex: Vec<String> = row.iter().map(|b| format!("0x{b:02X}")).collect();
            format!("    {}", hex.join(" "))
        });
        for dump in [xxd, hexdump, od, spaced] {
            assert!(
                super::longest_hex_dump(dump.as_bytes()) > MAX_LITERAL_RUN,
                "{dump:.80}"
            );
            let problems = check("notes/dump.txt", dump.as_bytes());
            assert!(
                problems.iter().any(|p| p.contains("hex dump")),
                "{dump:.80}: {problems:?}"
            );
        }
    }

    /// Only lines that are mostly hex values count, and any other line ends
    /// the block: prose, tables and code with the odd hex word or number
    /// are not dumps.
    #[test]
    fn hex_dumps_are_only_lines_of_hex_values() {
        let dump = super::longest_hex_dump;
        assert_eq!(dump(b"00000000: 4d5a 9000 0300 0000  MZ......\n"), 5);
        assert_eq!(dump(b"0x4d 0x5a 0x90:\n"), 0, "too few values");
        assert_eq!(dump(b"a bad cafe face was fed\n"), 5, "mostly hex");
        assert_eq!(
            dump(b"a bad cafe was fed by the dog\n"),
            0,
            "too many words"
        );
        assert_eq!(
            dump(b"1 2 3 4\nnot a dump\n5 6 7 8\n"),
            4,
            "other lines end the block"
        );
        assert_eq!(dump(b"1 2 3 4\r\n5 6 7 8\r\n"), 8, "CRLF lines join");
        assert_eq!(dump(b"1 2 3 4\n\n5 6 7 8\n"), 4, "a blank line ends it");
        assert_eq!(
            dump("fffffffffffffffff ".repeat(4).as_bytes()),
            0,
            "too long for values"
        );
        assert_eq!(dump(b""), 0);

        let table: String = (0..256)
            .map(|r| format!("| {r} | {} | {} |\n", r * 2, r % 7))
            .collect();
        assert_eq!(dump(table.as_bytes()), 0, "a markdown table of numbers");
        assert!(check("docs/table.md", table.as_bytes()).is_empty());
        let prose = "The decade-old cafe had a bead of dead beef on its face.\n".repeat(200);
        assert_eq!(dump(prose.as_bytes()), 0);
    }
}
