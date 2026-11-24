//! The lexical part of Windows path rewriting for virtual drives.
//!
//! [`super::files`] hands the title's paths to the host, but the decisions
//! about drive letters, separators and `\\?\` paths are pure UTF-16 work.
//! They live here so tests cover them on every platform, not only Windows.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub(crate) const SEPARATORS: [u16; 2] = [b'\\' as u16, b'/' as u16];
pub(crate) const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// Host paths from this length on take the `\\?\` form, which lifts the
/// host's MAX_PATH limit (248 characters for a directory, 260 for a file).
const LONG: usize = 248;

/// A mounted drive: upper-case letter and host root (UTF-16, no trailing
/// separator, no NUL).
pub(crate) type Mount = (u16, Vec<u16>);

/// Whether `bytes`, a narrow path, names a drive (possibly through `\\?\`).
/// The drive part is ASCII in every code page, so this needs no decoding.
pub(crate) fn has_drive(bytes: &[u8]) -> bool {
    matches!(
        bytes.strip_prefix(br"\\?\").unwrap_or(bytes),
        [letter, b':', ..] if letter.is_ascii_alphabetic()
    )
}

/// `root` and the title's `tail` as a `\\?\` path. That form turns off the
/// host's own clean-up, so `/` becomes `\` and `.` and `..` are resolved
/// here (`..` stops at the root).
pub(crate) fn verbatim(root: &[u16], tail: &[u16]) -> Vec<u16> {
    let backslash = u16::from(b'\\');
    let mut out = if root.starts_with(&VERBATIM) {
        root.to_vec()
    } else if let Some(unc) = root.strip_prefix(&[backslash; 2][..]) {
        let mut out = VERBATIM.to_vec();
        out.extend("UNC\\".encode_utf16());
        out.extend_from_slice(unc);
        out
    } else {
        [&VERBATIM[..], root].concat()
    };
    while out.last() == Some(&backslash) {
        out.pop();
    }
    let dot = u16::from(b'.');
    let mut parts: Vec<&[u16]> = Vec::new();
    for part in tail.split(|c| SEPARATORS.contains(c)) {
        match part {
            [] => {}
            [d] if *d == dot => {}
            [d, e] if [*d, *e] == [dot; 2] => drop(parts.pop()),
            _ => parts.push(part),
        }
    }
    for part in parts {
        out.push(backslash);
        out.extend_from_slice(part);
    }
    out
}

/// The host path, NUL-terminated, for `path` (without its NUL) if it is on
/// one of `mounts`. Every decision is lexical: no host calls, no filesystem.
pub(crate) fn translate(path: &[u16], mounts: &[Mount]) -> Option<Vec<u16>> {
    let (prefix, rest) = match path.strip_prefix(&VERBATIM[..]) {
        Some(rest) => (&VERBATIM[..], rest),
        None => (&[][..], path),
    };
    let &[letter, colon, ..] = rest else {
        return None;
    };
    if colon != u16::from(b':') {
        return None;
    }
    let letter = u8::try_from(letter).ok().filter(u8::is_ascii_alphabetic)?;
    let letter = u16::from(letter.to_ascii_uppercase());
    let (_, root) = mounts.iter().find(|(l, _)| *l == letter)?;
    let tail = &rest[2..];
    if prefix.is_empty() && root.len() + tail.len() >= LONG {
        let mut out = verbatim(root, tail);
        out.push(0);
        return Some(out);
    }
    let mut out = prefix.to_vec();
    out.extend_from_slice(root);
    // `X:` and `X:name` are relative to the root.
    if !tail.first().is_some_and(|c| SEPARATORS.contains(c)) {
        out.push(b'\\'.into());
    }
    let verbatim = !prefix.is_empty();
    out.extend(tail.iter().map(|&c| {
        if verbatim && c == u16::from(b'/') {
            b'\\'.into()
        } else {
            c
        }
    }));
    out.push(0);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
    fn text(s: &[u16]) -> String {
        String::from_utf16_lossy(&s[..s.len() - usize::from(s.last() == Some(&0))])
    }
    fn mounts() -> Vec<Mount> {
        vec![
            (u16::from(b'X'), wide(r"C:\games\title")),
            (u16::from(b'G'), wide(r"D:\package")),
            // A root that is already verbatim and one that is a UNC share.
            (
                u16::from(b'T'),
                [&VERBATIM[..], &wide(r"E:\temp")[..]].concat(),
            ),
            (u16::from(b'U'), wide(r"\\server\share")),
        ]
    }
    fn host(path: &str) -> String {
        text(&translate(&wide(path), &mounts()).expect("mounted drive"))
    }

    #[test]
    fn rewrites_drive_paths_and_their_case() {
        assert_eq!(host(r"X:\save\one.dat"), r"C:\games\title\save\one.dat");
        assert_eq!(host(r"x:\SAVE\one.dat"), r"C:\games\title\SAVE\one.dat");
        assert_eq!(host("X:/save/one.dat"), r"C:\games\title/save/one.dat");
        assert_eq!(host("G:\\"), r"D:\package\");
    }

    #[test]
    fn drives_without_a_separator_stay_relative_to_the_root() {
        assert_eq!(host("X:"), r"C:\games\title\");
        assert_eq!(host("X:save"), r"C:\games\title\save");
        assert_eq!(host("x:save/one.dat"), r"C:\games\title\save/one.dat");
    }

    #[test]
    fn unmounted_and_malformed_paths_are_left_alone() {
        assert!(translate(&wide(r"C:\Windows"), &mounts()).is_none());
        assert!(translate(&wide(r"1:\bad"), &mounts()).is_none());
        assert!(translate(&wide(r"X"), &mounts()).is_none());
        assert!(translate(&[], &mounts()).is_none());
        assert!(translate(&wide(r"Y:\unmounted"), &mounts()).is_none());
        assert!(translate(&wide(r"\\?\C:\Windows"), &mounts()).is_none());
    }

    #[test]
    fn verbatim_paths_keep_their_prefix_and_use_backslashes() {
        assert_eq!(
            host(r"\\?\X:\save/one.dat"),
            r"\\?\C:\games\title\save\one.dat"
        );
        // A short verbatim path keeps its dots: the host's own `\\?\`
        // handling passes them through, and only long paths are cleaned up.
        assert_eq!(
            host(r"\\?\x:\save/./two/../one.dat"),
            r"\\?\C:\games\title\save\.\two\..\one.dat"
        );
        // An already-verbatim root is not prefixed again.
        assert_eq!(host(r"T:\file"), r"\\?\E:\temp\file");
    }

    #[test]
    fn long_paths_take_the_verbatim_form_and_resolve_dots() {
        let tail = format!("X:{}\\one.dat", "a".repeat(LONG));
        assert_eq!(
            host(&tail),
            format!(r"\\?\C:\games\title\{}\one.dat", "a".repeat(LONG))
        );
        let tail = format!("X:{}\\.\\up\\..\\one.dat", "a".repeat(LONG));
        assert_eq!(
            host(&tail),
            format!(r"\\?\C:\games\title\{}\one.dat", "a".repeat(LONG))
        );
        // `..` stops at the root instead of escaping it.
        let tail = format!("X:{}\\..\\..\\one.dat", "a".repeat(LONG));
        assert_eq!(host(&tail), format!(r"\\?\C:\games\title\one.dat"));
        // Just under the limit stays in the ordinary form.
        let tail = format!("X:{}", "a".repeat(LONG - r"C:\games\title".len() - 1));
        assert!(host(&tail).starts_with(r"C:\games\title"));
        assert!(!host(&tail).starts_with(r"\\?\"));
    }

    #[test]
    fn unc_roots_become_verbatim_unc() {
        let tail = format!("U:{}\\{}", "a".repeat(LONG), "file");
        assert_eq!(
            host(&tail),
            format!(r"\\?\UNC\server\share\{}\file", "a".repeat(LONG))
        );
    }

    #[test]
    fn narrow_drive_detection_precedes_decoding() {
        assert!(has_drive(br"X:\save"));
        assert!(has_drive(br"x:save"));
        assert!(has_drive(br"\\?\X:\save"));
        assert!(!has_drive(br"\Windows"));
        assert!(!has_drive(br"\\server\share"));
        assert!(!has_drive(b""));
    }
}
