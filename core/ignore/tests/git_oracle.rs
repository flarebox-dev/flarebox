// SPDX-License-Identifier: Apache-2.0

//! Differential tests against `git check-ignore`: random trees with random rules files must get
//! the same verdict from this crate and from git, down to the file and line that excludes each
//! path. Each tree is written to disk as a git work tree with `.gitignore` files, and the same
//! bytes are given to the matcher as `.fbignore` files.
//!
//! The generators leave out the intentional differences listed in the crate documentation: the
//! rules files themselves are not compared, names and patterns are ASCII, and under case folding
//! patterns hold no uppercase bracket members or escaped uppercase letters. A third run spells
//! names and patterns with non-ASCII letters NFD for this crate and NFC for git, without `?`
//! and bracket expressions, which git applies to bytes.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use flarebox_ignore::{Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Status};
use proptest::prelude::*;
use proptest::sample::Index;
use unicode_normalization::UnicodeNormalization;

struct Exact;

impl NameKey for Exact {
    fn key(name: &str) -> String {
        name.to_owned()
    }
}

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        flarebox_engine::names::name_key(name)
    }
}

/// A tree of entries and the rules files in some of its directories.
#[derive(Clone, Debug)]
struct Case {
    entries: BTreeMap<Vec<String>, Kind>,
    files: BTreeMap<Vec<String>, String>,
}

/// What excluded a path: the depth of the rules file's directory and the line, or `None` when
/// the path is included.
type Verdict = Option<(usize, u32)>;

/// This crate's verdicts, with every name and rules file respelled by `spell` first.
fn ours<K: NameKey>(case: &Case, spell: fn(&str) -> String) -> Vec<Verdict> {
    let respell = |path: &[String]| -> Vec<String> { path.iter().map(|n| spell(n)).collect() };
    let mut matcher = Matcher::<K>::new(LocalRules::default());
    for (dir, text) in &case.files {
        let rules = RuleFile::parse(spell(text).as_bytes());
        matcher.set_rules(&respell(dir), Some(rules), []);
    }
    case.entries
        .iter()
        .map(|(path, kind)| match matcher.status(&respell(path), *kind) {
            Status::Included => None,
            Status::Excluded(Reason::Rule { depth, line }) => Some((depth, line)),
            Status::Excluded(reason) => panic!("{path:?} excluded by {reason:?}"),
        })
        .collect()
}

fn git(case: &Case, ignorecase: bool) -> Vec<Verdict> {
    let work = tempfile::tempdir().expect("temporary directory");
    let root = work.path();
    for (path, kind) in &case.entries {
        let at = path
            .iter()
            .fold(root.to_path_buf(), |at, name| at.join(name));
        match kind {
            Kind::Dir => std::fs::create_dir_all(&at),
            Kind::File => std::fs::write(&at, b""),
        }
        .unwrap_or_else(|e| panic!("cannot create {at:?}: {e}"));
    }
    for (dir, text) in &case.files {
        let at = dir
            .iter()
            .fold(root.to_path_buf(), |at, name| at.join(name));
        std::fs::write(at.join(".gitignore"), text).expect("write .gitignore");
    }
    run_git(root, &["init", "-q"], None);

    let mut input = Vec::new();
    for path in case.entries.keys() {
        input.extend_from_slice(path.join("/").as_bytes());
        input.push(0);
    }
    let ignorecase = format!("core.ignorecase={ignorecase}");
    let output = run_git(
        root,
        &[
            "-c",
            &ignorecase,
            "check-ignore",
            "--no-index",
            "--verbose",
            "--non-matching",
            "-z",
            "--stdin",
        ],
        Some(&input),
    );
    let fields: Vec<&[u8]> = output.split(|&b| b == 0).collect();
    let records: Vec<&[&[u8]]> = fields.chunks(4).filter(|r| r.len() == 4).collect();
    assert_eq!(
        records.len(),
        case.entries.len(),
        "git check-ignore output: {:?}",
        String::from_utf8_lossy(&output)
    );
    records
        .into_iter()
        .zip(case.entries.keys())
        .map(|(record, path)| {
            let [source, line, pattern, echoed] = record else {
                unreachable!()
            };
            assert_eq!(
                *echoed,
                path.join("/").as_bytes(),
                "git answered out of order"
            );
            if source.is_empty() || pattern.starts_with(b"!") {
                return None;
            }
            let source = std::str::from_utf8(source).expect("UTF-8 source");
            let depth = source.matches('/').count();
            let line = std::str::from_utf8(line)
                .expect("line")
                .parse()
                .expect("line number");
            Some((depth, line))
        })
        .collect()
}

fn run_git(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the git oracle needs git on PATH");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.unwrap_or_default())
        .expect("write to git");
    let output = child.wait_with_output().expect("git output");
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn same(text: &str) -> String {
    text.to_owned()
}

fn nfd(text: &str) -> String {
    text.nfd().collect()
}

fn compare<K: NameKey>(
    case: &Case,
    ignorecase: bool,
    spell: fn(&str) -> String,
) -> Result<(), TestCaseError> {
    let ours = ours::<K>(case, spell);
    let theirs = git(case, ignorecase);
    for ((path, kind), (ours, theirs)) in case.entries.iter().zip(ours.into_iter().zip(theirs)) {
        prop_assert_eq!(
            ours,
            theirs,
            "{:?} ({:?}) with rules files {:#?}",
            path,
            kind,
            case.files
        );
    }
    Ok(())
}

/// Git's `trim_trailing_spaces`, to see a line as git compiles it.
fn trimmed(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut last_space = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' => {
                last_space.get_or_insert(i);
            }
            b'\\' => {
                i += 1;
                if i == bytes.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    &line[..last_space.unwrap_or(line.len())]
}

/// Whether the line has a `**` right after the literal start of a pattern with a slash
/// (`foo**/bar`). Git 2.39 treats that `**` as special, git 2.54 as a plain `*`, as git
/// documents and this crate does, so the oracle leaves such lines out.
fn version_dependent(line: &str) -> bool {
    let line = trimmed(line);
    let body = line.strip_prefix('!').unwrap_or(line);
    let body = body.strip_suffix('/').unwrap_or(body);
    if !body.contains('/') {
        return false;
    }
    let body = body.strip_prefix('/').unwrap_or(body).as_bytes();
    let Some(prefix) = body.iter().position(|b| b"*?[\\".contains(b)) else {
        return false;
    };
    if prefix == 0 || body[prefix - 1] == b'/' || !body[prefix..].starts_with(b"**") {
        return false;
    }
    let end = prefix + body[prefix..].iter().take_while(|&&b| b == b'*').count();
    match body.get(end) {
        None | Some(b'/') => true,
        Some(b'\\') => body.get(end + 1) == Some(&b'/'),
        Some(_) => false,
    }
}

fn name() -> impl Strategy<Value = String> {
    "[abAB.*?\\[\\]\\\\!# -]{1,3}".prop_filter("not . or ..", |n| n != "." && n != "..")
}

/// Bracket expressions. Under folding, members are lowercase: git leaves them unfolded.
fn class(fold: bool) -> BoxedStrategy<String> {
    let mut classes = vec![
        "[ab]",
        "[!a]",
        "[^b]",
        "[a-c]",
        "[A-C]",
        "[Z-a]",
        "[]a]",
        "[!]]",
        "[a-]",
        "[-b]",
        "[*?]",
        "[\\]]",
        "[\\a-b]",
        "[a-c-e]",
        "[[:alpha:]]",
        "[[:upper:]]",
        "[[:lower:]]",
        "[[:punct:][:digit:]]",
        "[[:space:]]",
        "[[:alnum:]x]",
        "[[:]]",
        "[[:a]",
        "[b-a]",
        "[.-]",
        "[[]",
        "[ab",
        "[[:nope:]]",
        "[\\",
    ];
    if !fold {
        classes.extend(["[A]", "[aB]", "[!B]", "[\\A]", "[[:upper:]A]"]);
    }
    proptest::sample::select(classes)
        .prop_map(str::to_owned)
        .boxed()
}

fn piece(fold: bool) -> impl Strategy<Value = String> {
    let mut literals = vec![
        "a", "b", "A", "B", ".", "-", " ", "#", "!", "\\*", "\\?", "\\[", "\\]", "\\\\", "\\!",
        "\\#", "\\ ", "\\a", "\\-",
    ];
    if !fold {
        literals.push("\\A");
    }
    prop_oneof![
        4 => proptest::sample::select(literals).prop_map(str::to_owned),
        2 => Just("*".to_owned()),
        1 => Just("**".to_owned()),
        1 => Just("***".to_owned()),
        2 => Just("?".to_owned()),
        2 => class(fold),
    ]
}

fn structured(fold: bool) -> impl Strategy<Value = String> {
    (
        any::<bool>(),
        prop_oneof![3 => Just(""), 1 => Just("/"), 1 => Just("\\/")],
        prop::collection::vec(prop::collection::vec(piece(fold), 1..4), 1..4),
        prop_oneof![3 => Just(""), 1 => Just("/"), 1 => Just("//")],
        prop_oneof![3 => Just(""), 1 => Just(" "), 1 => Just("  "), 1 => Just("\\ "), 1 => Just("\t")],
    )
        .prop_map(|(negated, lead, segments, trail, spaces)| {
            let body: Vec<String> = segments.into_iter().map(|s| s.concat()).collect();
            format!(
                "{}{lead}{}{trail}{spaces}",
                if negated { "!" } else { "" },
                body.join("/")
            )
        })
}

fn line(fold: bool) -> impl Strategy<Value = String> {
    let raw = if fold {
        "[ab*?\\[\\]\\\\/!# :-]{0,8}"
    } else {
        "[aAb*?\\[\\]\\\\/!# :-]{0,8}"
    };
    prop_oneof![
        6 => structured(fold),
        2 => raw,
        1 => Just(String::new()),
        1 => Just("# comment".to_owned()),
    ]
    .prop_filter("version-dependent in git", |line| !version_dependent(line))
}

/// Names and patterns with non-ASCII letters, for comparing NFD spellings here with NFC ones in
/// git. Git matches bytes, so `?` and bracket expressions are left out.
fn unicode_name() -> impl Strategy<Value = String> {
    "[aAéÉüÅ.-]{1,3}".prop_filter("not . or ..", |n| n != "." && n != "..")
}

fn unicode_line() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        4 => proptest::sample::select(vec!["a", "A", "é", "É", "ü", "Å", "\\é", "-", "."])
            .prop_map(str::to_owned),
        2 => Just("*".to_owned()),
        1 => Just("**".to_owned()),
    ];
    (
        any::<bool>(),
        prop_oneof![3 => Just(""), 1 => Just("/")],
        prop::collection::vec(prop::collection::vec(piece, 1..4), 1..4),
        prop_oneof![3 => Just(""), 1 => Just("/")],
    )
        .prop_map(|(negated, lead, segments, trail)| {
            let body: Vec<String> = segments.into_iter().map(|s| s.concat()).collect();
            format!(
                "{}{lead}{}{trail}",
                if negated { "!" } else { "" },
                body.join("/")
            )
        })
        .prop_filter("version-dependent in git", |line| !version_dependent(line))
}

fn case(fold: bool) -> impl Strategy<Value = Case> {
    case_with(name().boxed(), line(fold).boxed())
}

fn case_with(
    name: BoxedStrategy<String>,
    line: BoxedStrategy<String>,
) -> impl Strategy<Value = Case> {
    let paths = prop::collection::vec((prop::collection::vec(name, 1..5), any::<bool>()), 1..30);
    let files = prop::collection::vec(
        (
            any::<Index>(),
            prop::collection::vec(line, 0..8),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        ),
        0..5,
    );
    (paths, files).prop_map(|(paths, files)| {
        let mut entries: BTreeMap<Vec<String>, Kind> = BTreeMap::new();
        // Siblings that differ only by case would collide on a case-insensitive volume, so a later
        // spelling takes the first one's.
        let mut spellings: BTreeMap<(Vec<String>, String), String> = BTreeMap::new();
        'paths: for (raw, is_dir) in paths {
            let mut path = Vec::new();
            for (i, name) in raw.iter().enumerate() {
                let spelling = spellings
                    .entry((path.clone(), name.to_lowercase()))
                    .or_insert_with(|| name.clone())
                    .clone();
                path.push(spelling);
                let kind = if i + 1 < raw.len() || is_dir {
                    Kind::Dir
                } else {
                    Kind::File
                };
                match entries.get(&path) {
                    Some(Kind::File) if i + 1 < raw.len() => continue 'paths,
                    Some(_) => {}
                    None => {
                        entries.insert(path.clone(), kind);
                    }
                }
            }
        }
        let dirs: Vec<Vec<String>> = std::iter::once(Vec::new())
            .chain(
                entries
                    .iter()
                    .filter(|(_, k)| **k == Kind::Dir)
                    .map(|(p, _)| p.clone()),
            )
            .collect();
        let files = files
            .into_iter()
            .map(|(dir, lines, crlf, bom, final_newline)| {
                let mut text = String::new();
                if bom {
                    text.push('\u{feff}');
                }
                text.push_str(&lines.join(if crlf { "\r\n" } else { "\n" }));
                if final_newline {
                    text.push('\n');
                }
                (dir.get(&dirs).clone(), text)
            })
            .collect();
        Case { entries, files }
    })
}

proptest! {
    #[test]
    fn agrees_with_git_case_sensitive(case in case(false)) {
        compare::<Exact>(&case, false, same)?;
    }

    #[test]
    fn agrees_with_git_ignorecase(case in case(true)) {
        compare::<Folded>(&case, true, same)?;
    }

    /// Names and rules files spelled NFD here and NFC in git get the same verdicts.
    #[test]
    fn nfd_spellings_agree_with_git_on_nfc(case in case_with(unicode_name().boxed(), unicode_line().boxed())) {
        compare::<Exact>(&case, false, nfd)?;
    }
}

/// Hand-picked lines, one rules file each, on one fixed tree. Readable failures for the cases the
/// random generator only reaches by chance.
#[test]
fn curated_lines_agree_with_git() {
    let tree = [
        "a/b/c.txt",
        "a/b/d/",
        "a/x.log",
        "a/foo/bar",
        "a/foo/x/bar",
        "a/foox/bar",
        "foo/bar",
        "foo/a/b/bar",
        "b.log",
        "#x",
        "!x",
        " lead",
        "trail ",
        "[x]",
        "a*b",
        "q?",
        "back\\slash",
        "x/y/z/",
        "x/y/w",
        "abc/",
        "abc/def/",
        "abc/def/ghi",
        "Upper/Case.TXT",
    ];
    let lines = [
        "*.log",
        "/b.log",
        "a/b/",
        "a/b",
        "b/",
        "foo/**/bar",
        "**/bar",
        "a/**",
        "**",
        "*",
        "a/*",
        "a/**/",
        "**/b/**",
        "a/**/b",
        "\\#x",
        "\\!x",
        "#x",
        "!x",
        " lead",
        "trail\\ ",
        "trail ",
        "\\[x\\]",
        "[[]x]",
        "a\\*b",
        "a*b",
        "q\\?",
        "back\\\\slash",
        "[a-c]*",
        "[!a]*",
        "x/*/z",
        "x/*",
        "/x/y/",
        "abc/**/ghi",
        "abc/***/ghi",
        "a**",
        "**/",
        "*/",
        "/*",
        "?",
        "??",
        "[[:alpha:]]*",
        "[[:upper:]]*",
        "[[:digit:][:punct:]]*",
        "upper/case.txt",
        "UPPER/",
        "*.TXT",
        "a/b/c.txt/",
        "x/y/z/w",
    ];
    let mut entries = BTreeMap::new();
    for path in tree {
        let kind = if path.ends_with('/') {
            Kind::Dir
        } else {
            Kind::File
        };
        let parts: Vec<String> = path
            .trim_end_matches('/')
            .split('/')
            .map(str::to_owned)
            .collect();
        for depth in 1..parts.len() {
            entries.insert(parts[..depth].to_vec(), Kind::Dir);
        }
        entries.insert(parts, kind);
    }
    for line in lines {
        for text in [format!("{line}\n"), format!("{line}\n!*.txt\n!a/\n")] {
            let case = Case {
                entries: entries.clone(),
                files: BTreeMap::from([(Vec::new(), text.clone())]),
            };
            for fold in [false, true] {
                let ours = if fold {
                    ours::<Folded>(&case, same)
                } else {
                    ours::<Exact>(&case, same)
                };
                let theirs = git(&case, fold);
                for ((path, _), (o, t)) in case.entries.iter().zip(ours.iter().zip(&theirs)) {
                    assert_eq!(o, t, "rules {text:?}, path {path:?}, folding {fold}");
                }
            }
        }
    }
}
