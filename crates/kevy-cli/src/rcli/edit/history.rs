//! Command history: the last 100 lines in memory, and a file that never
//! receives a line carrying a credential.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Lines kept, as redis-cli keeps.
pub(crate) const MAX_ENTRIES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    line: Vec<u8>,
    sensitive: bool,
}

/// In-memory history, oldest first.
#[derive(Debug, Default)]
pub(crate) struct History {
    entries: VecDeque<Entry>,
}

impl History {
    /// Add a line unless it repeats the newest one.
    pub(crate) fn add(&mut self, line: &[u8], sensitive: bool) {
        if self.entries.back().is_some_and(|e| e.line == line) {
            return;
        }
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry { line: line.to_vec(), sensitive });
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The line `back` steps from the newest (0 = newest).
    pub(crate) fn newest(&self, back: usize) -> Option<&[u8]> {
        let i = self.entries.len().checked_sub(back + 1)?;
        self.entries.get(i).map(|e| e.line.as_slice())
    }

    /// Load `path`: one line per entry, CR/LF stripped. A missing file is an
    /// empty history, not an error.
    pub(crate) fn load(&mut self, path: &Path) {
        let Ok(text) = std::fs::read(path) else { return };
        for line in text.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !line.is_empty() {
                self.add(line, false);
            }
        }
    }

    /// Write every non-sensitive entry to `path`, readable by the owner only.
    /// The file is replaced whole via a temporary and a rename, so a failed
    /// write leaves the previous history rather than half of a new one.
    pub(crate) fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension("kevycli-tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        for e in self.entries.iter().filter(|e| !e.sensitive) {
            file.write_all(&e.line)?;
            file.write_all(b"\n")?;
        }
        std::fs::rename(&tmp, path)
    }
}

/// Where history lives: the first non-empty `KEVYCLI_HISTFILE`,
/// `VALKEYCLI_HISTFILE` or `REDISCLI_HISTFILE`, else `~/.kevycli_history`.
/// `/dev/null` means in memory only.
pub(crate) fn history_path(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let named = ["KEVYCLI_HISTFILE", "VALKEYCLI_HISTFILE", "REDISCLI_HISTFILE"]
        .iter()
        .find_map(|k| env(k).filter(|v| !v.is_empty()));
    match named {
        Some(p) if p == "/dev/null" => None,
        Some(p) => Some(PathBuf::from(p)),
        None => {
            env("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".kevycli_history"))
        }
    }
}

/// Whether a command carries a credential and so must stay out of the file.
///
/// The union of what redis-cli and valkey-cli withhold: withholding one line
/// too many costs a history entry, one too few leaks a password to disk.
pub(crate) fn is_sensitive(argv: &[Vec<u8>]) -> bool {
    let is = |i: usize, w: &str| argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(w.as_bytes()));
    let any = |i: usize, ws: &[&str]| ws.iter().any(|w| is(i, w));
    let n = argv.len();
    if is(0, "auth") {
        return true;
    }
    if n > 1 && is(0, "acl") && any(1, &["deluser", "setuser", "getuser"]) {
        return true;
    }
    if n > 2 && is(0, "config") && is(1, "set") {
        const KEYS: &[&str] = &[
            "masterauth",
            "masteruser",
            "primaryauth",
            "primaryuser",
            "requirepass",
            "tls-key-file-pass",
            "tls-client-key-file-pass",
        ];
        return (2..n).step_by(2).any(|j| any(j, KEYS));
    }
    if n > 4 && is(0, "hello") {
        return hello_carries_auth(argv);
    }
    if n > 7 && is(0, "migrate") {
        return migrate_carries_auth(argv);
    }
    n > 4
        && is(0, "sentinel")
        && ((is(1, "config") && is(2, "set") && any(3, &["sentinel-pass", "sentinel-user"]))
            || (is(1, "set") && any(3, &["auth-pass", "auth-user"])))
}

/// `HELLO <ver> [SETNAME name] AUTH user pass …`: options scanned in order.
fn hello_carries_auth(argv: &[Vec<u8>]) -> bool {
    let mut j = 2;
    while j < argv.len() {
        let more = argv.len() - 1 - j;
        match argv[j].to_ascii_lowercase().as_slice() {
            b"auth" if more >= 2 => return true,
            b"setname" if more >= 1 => j += 2,
            _ => return false,
        }
    }
    false
}

/// `MIGRATE … [AUTH pass] [AUTH2 user pass] [KEYS …]`: credentials before KEYS.
fn migrate_carries_auth(argv: &[Vec<u8>]) -> bool {
    for j in 6..argv.len() {
        let more = argv.len() - 1 - j;
        match argv[j].to_ascii_lowercase().as_slice() {
            b"auth" if more >= 1 => return true,
            b"auth2" if more >= 2 => return true,
            b"keys" if more >= 1 => return false,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<Vec<u8>> {
        line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
    }

    #[test]
    fn sensitive_commands() {
        for yes in [
            "AUTH p",
            "auth u p",
            "ACL SETUSER u on",
            "acl getuser u",
            "CONFIG SET maxmemory 1 requirepass x",
            "config set primaryauth x",
            "HELLO 3 AUTH u p",
            "HELLO 3 SETNAME n AUTH u p",
            "MIGRATE h 1 k 0 5 COPY AUTH p",
            "MIGRATE h 1 \"\" 0 5 AUTH2 u p KEYS a",
            "SENTINEL CONFIG SET sentinel-pass p",
            "SENTINEL SET m auth-user u",
        ] {
            assert!(is_sensitive(&argv(yes)), "{yes}");
        }
        for no in [
            "GET auth",
            "ACL LIST",
            "CONFIG SET maxmemory 1",
            "CONFIG GET requirepass",
            "HELLO 3 SETNAME n",
            "HELLO 3 FOO a b",
            "MIGRATE h 1 k 0 5 KEYS auth x",
            "SENTINEL SET m quorum 2",
            "SENTINEL CONFIG GET a b",
        ] {
            assert!(!is_sensitive(&argv(no)), "{no}");
        }
    }

    #[test]
    fn ring_of_a_hundred_without_repeats() {
        let mut h = History::default();
        h.add(b"a", false);
        h.add(b"a", false);
        assert_eq!(h.len(), 1);
        for i in 0..150 {
            h.add(format!("c{i}").as_bytes(), false);
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(h.newest(0), Some(&b"c149"[..]));
        assert_eq!(h.newest(99), Some(&b"c50"[..]));
        assert_eq!(h.newest(100), None);
    }

    #[test]
    fn file_round_trip_skips_secrets_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kevy-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir is writable in tests");
        let path = dir.join("h");
        let mut h = History::default();
        h.add(b"GET k", false);
        h.add(b"AUTH secret", true);
        h.add(b"SET k v", false);
        h.save(&path).expect("temp dir is writable in tests");
        assert_eq!(std::fs::read(&path).ok(), Some(b"GET k\nSET k v\n".to_vec()));
        let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777).ok();
        assert_eq!(mode, Some(0o600));
        std::fs::write(&path, b"one\r\n\ntwo\n").expect("temp dir is writable in tests");
        let mut loaded = History::default();
        loaded.load(&path);
        loaded.load(&dir.join("missing"));
        assert_eq!((loaded.newest(0), loaded.newest(1)), (Some(&b"two"[..]), Some(&b"one"[..])));
        assert!(h.save(&dir.join("no-such-dir/h")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn where_history_lives() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        assert_eq!(
            history_path(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.kevycli_history"))
        );
        assert_eq!(
            history_path(env(&[("REDISCLI_HISTFILE", "/r"), ("HOME", "/h")])),
            Some(PathBuf::from("/r"))
        );
        assert_eq!(
            history_path(env(&[("KEVYCLI_HISTFILE", ""), ("VALKEYCLI_HISTFILE", "/v")])),
            Some(PathBuf::from("/v"))
        );
        assert_eq!(history_path(env(&[("REDISCLI_HISTFILE", "/dev/null"), ("HOME", "/h")])), None);
        assert_eq!(history_path(env(&[])), None);
    }
}
