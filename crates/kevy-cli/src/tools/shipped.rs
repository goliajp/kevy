//! The tools kevy-cli shipped as bare words before `--kevy` existed. Under
//! `--kevy` they run on the session's connection; as bare words they keep
//! their 6.4 form for the rest of 6.x, with a deprecation line (RFC §13.5).

/// One shipped tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shipped {
    Sql,
    Export,
    Import,
    Backup,
    Restore,
    Doctor,
    Shadow,
    Lint,
    BackfillKeys,
    CopyPrefix,
    DeletePrefix,
    Digest,
    Diff,
    Inspect,
}

const NAMES: &[(&str, Shipped)] = &[
    ("sql", Shipped::Sql),
    ("export", Shipped::Export),
    ("import", Shipped::Import),
    ("backup", Shipped::Backup),
    ("restore", Shipped::Restore),
    ("doctor", Shipped::Doctor),
    ("shadow", Shipped::Shadow),
    ("lint", Shipped::Lint),
    ("backfill-keys", Shipped::BackfillKeys),
    ("copy-prefix", Shipped::CopyPrefix),
    ("delete-prefix", Shipped::DeletePrefix),
    ("digest", Shipped::Digest),
    ("diff", Shipped::Diff),
    ("inspect", Shipped::Inspect),
];

impl Shipped {
    pub(crate) fn named(name: &[u8]) -> Option<Shipped> {
        NAMES.iter().find(|(n, _)| n.as_bytes() == name).map(|(_, t)| *t)
    }

    pub(crate) fn name(self) -> &'static str {
        NAMES.iter().find(|(_, t)| *t == self).map_or("", |(n, _)| n)
    }

    /// Whether this run talks to a server: the backup pair works on files,
    /// and `sql` only for `compile --apply`.
    pub(crate) fn needs_server(self, args: &[String]) -> bool {
        match self {
            Shipped::Backup | Shipped::Restore => false,
            Shipped::Sql => {
                args.first().is_some_and(|s| s == "compile") && args.iter().any(|a| a == "--apply")
            }
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Shipped;

    #[test]
    fn every_shipped_name_reads_back() {
        for name in [
            "sql",
            "export",
            "import",
            "backup",
            "restore",
            "doctor",
            "shadow",
            "lint",
            "backfill-keys",
            "copy-prefix",
            "delete-prefix",
            "digest",
            "diff",
            "inspect",
        ] {
            let tool = Shipped::named(name.as_bytes()).expect("a shipped name");
            assert_eq!(tool.name(), name);
        }
        assert!(Shipped::named(b"tables").is_none());
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!Shipped::Sql.needs_server(&args(&["plan", "f.sql"])));
        assert!(Shipped::Sql.needs_server(&args(&["compile", "f.sql", "--apply"])));
        assert!(!Shipped::Backup.needs_server(&[]));
        assert!(Shipped::Digest.needs_server(&args(&["p:"])));
    }
}
