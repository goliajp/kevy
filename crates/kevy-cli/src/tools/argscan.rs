//! Strict reading of a tool's arguments: a flag without its value, a
//! number that does not parse and a word the tool does not take are each
//! refused by name — never skipped, never replaced by a default.

use std::str::FromStr;

/// A cursor over one tool's arguments.
pub(crate) struct Scan<'a> {
    args: &'a [String],
    i: usize,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(args: &'a [String]) -> Scan<'a> {
        Scan { args, i: 0 }
    }

    /// The next word, flag or not.
    pub(crate) fn next(&mut self) -> Option<&'a str> {
        let word = self.args.get(self.i)?;
        self.i += 1;
        Some(word)
    }

    /// The value after `flag`.
    pub(crate) fn value(&mut self, flag: &str) -> Result<&'a str, String> {
        self.next().ok_or_else(|| format!("{flag} needs a value"))
    }

    /// The value after `flag`, as a number.
    pub(crate) fn number<T: FromStr>(&mut self, flag: &str) -> Result<T, String> {
        let text = self.value(flag)?;
        text.parse().map_err(|_| format!("{flag} takes a number, not '{text}'"))
    }
}

/// The refusal for a word a tool does not take. redis-cli's connection
/// options are named as such: under `--kevy` they belong before it.
pub(crate) fn unexpected(word: &str) -> String {
    const CONNECTION: &[&str] = &["-h", "-p", "-s", "-u", "-a", "-n", "--user", "--pass", "--url"];
    if CONNECTION.contains(&word) {
        format!("{word} is a connection option; give it before --kevy")
    } else {
        format!("unexpected '{word}'")
    }
}

#[cfg(test)]
mod tests {
    use super::{Scan, unexpected};

    #[test]
    fn values_and_numbers_are_refused_by_name() {
        let args: Vec<String> = ["--rate", "x", "--rate"].iter().map(|s| s.to_string()).collect();
        let mut scan = Scan::new(&args);
        assert_eq!(scan.next(), Some("--rate"));
        assert_eq!(scan.number::<u64>("--rate"), Err("--rate takes a number, not 'x'".into()));
        assert_eq!(scan.next(), Some("--rate"));
        assert_eq!(scan.value("--rate"), Err("--rate needs a value".into()));
        assert_eq!(unexpected("-p"), "-p is a connection option; give it before --kevy");
        assert_eq!(unexpected("--bogus"), "unexpected '--bogus'");
    }
}
