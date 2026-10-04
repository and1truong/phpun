use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A parsed .phpt file.
#[derive(Debug)]
pub struct PhptTest {
    pub path: PathBuf,
    /// Section name → verbatim text (raw newlines preserved).
    pub sections: BTreeMap<String, String>,
    /// Unknown section markers the parser rejected.
    pub borked: Option<String>,
}

/// Sections the harness recognizes. Unknown `--NAME--` markers mark the
/// test borked, like run-tests.php does.
pub const ALLOWED_SECTIONS: &[&str] = &[
    "TEST",
    "EXPECT",
    "EXPECTF",
    "EXPECTREGEX",
    "EXPECTREGEX_EXTERNAL",
    "EXPECT_EXTERNAL",
    "EXPECTF_EXTERNAL",
    "EXPECTHEADERS",
    "POST",
    "POST_RAW",
    "GZIP_POST",
    "DEFLATE_POST",
    "PUT",
    "GET",
    "COOKIE",
    "ARGS",
    "FILE",
    "FILEEOF",
    "FILE_EXTERNAL",
    "REDIRECTTEST",
    "CAPTURE_STDIO",
    "STDIN",
    "CGI",
    "PHPDBG",
    "INI",
    "ENV",
    "EXTENSIONS",
    "SKIPIF",
    "XFAIL",
    "XLEAK",
    "CLEAN",
    "CREDITS",
    "DESCRIPTION",
    "CONFLICTS",
    "WHITESPACE_SENSITIVE",
    "FLAKY",
];

/// Byte-preserving decode: phpt files are byte strings (EXPECT bodies
/// can contain raw bytes like \xff; FILE sources can be Latin-1), so
/// map byte n to codepoint n. Everything downstream compares and
/// re-encodes in this same space (run-tests.php treats them as bytes).
pub fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Inverse of latin1_to_string — chars > 0xff cannot appear in a
/// latin1-decoded section, but map defensively to '?'.
pub fn string_to_latin1(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| if (c as u32) <= 0xff { c as u8 } else { b'?' })
        .collect()
}

pub fn parse_file(path: &Path) -> std::io::Result<PhptTest> {
    let raw = std::fs::read(path)?;
    let content = latin1_to_string(&raw);
    Ok(parse_str(path.to_path_buf(), &content))
}

pub fn parse_str(path: PathBuf, content: &str) -> PhptTest {
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut borked: Option<String> = None;
    let mut sec_done = false;
    let mut first = true;

    for line in content.split_inclusive('\n') {
        let trimmed_nl = line.trim_end_matches(['\n', '\r']);
        if first {
            first = false;
            if !trimmed_nl.starts_with("--TEST--") {
                borked = Some("tests must start with --TEST--".into());
                break;
            }
            current = Some("TEST".into());
            sections.insert("TEST".into(), String::new());
            continue;
        }
        if let Some(name) = section_header(trimmed_nl) {
            if sections.contains_key(name) {
                borked = Some(format!("duplicated {} section", name));
                break;
            }
            if !ALLOWED_SECTIONS.contains(&name) {
                borked = Some(format!("Unknown section \"{}\"", name));
                break;
            }
            sections.insert(name.to_string(), String::new());
            sec_done = false;
            current = Some(name.to_string());
            continue;
        }
        if !sec_done {
            if let Some(s) = current.as_deref() {
                sections.get_mut(s).unwrap().push_str(line);
            }
        }
        if matches!(
            current.as_deref(),
            Some("FILE") | Some("FILEEOF") | Some("FILE_EXTERNAL")
        ) && trimmed_nl == "===DONE==="
        {
            sec_done = true;
        }
    }

    // FILEEOF: FILE content = whole file after marker (already accumulated).
    if let Some(f) = sections.get("FILEEOF").cloned() {
        let f = f.trim_end_matches(['\r', '\n']).to_string();
        sections.insert("FILE".into(), f);
        sections.remove("FILEEOF");
    }

    PhptTest {
        path,
        sections,
        borked,
    }
}

fn section_header(line: &str) -> Option<&str> {
    if line.len() < 5 || !line.starts_with("--") || !line.ends_with("--") {
        return None;
    }
    let inner = &line[2..line.len() - 2];
    if inner.len() >= 2 && inner.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        Some(inner)
    } else {
        None
    }
}

impl PhptTest {
    pub fn has(&self, name: &str) -> bool {
        self.sections.contains_key(name)
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.sections.get(name).map(|s| s.as_str())
    }
    pub fn name(&self) -> String {
        self.get("TEST")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}
