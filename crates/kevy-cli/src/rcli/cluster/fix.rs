//! `--cluster fix`: the check, repairing what each stage finds as it goes —
//! open slots, uncovered slots, and (when searched for) slots with several
//! owners.

use super::addr;
use super::check::{self, Listing};
use super::config::Config;
use super::log::{self, Level};
use super::topology::Cluster;
use crate::rcli::opts::Opts;

/// Run `fix`; the exit code.
pub(crate) fn run(opts: &Opts, cfg: Config, args: &[Vec<u8>]) -> u8 {
    let Some(entry) = addr::entry(args) else { return addr::report_invalid() };
    let Some(mut c) = Cluster::load(opts, cfg, &entry) else { return 1 };
    super::show::info(&mut c);
    check::header(&c, Listing::Show);
    check::agreement(&c);
    for slot in check::open_slots(&c) {
        super::fix_open::fix(&mut c, slot);
    }
    if !check::coverage(&c) {
        if c.unreachable_masters > 0 && !c.cfg.fix_with_unreachable_masters {
            refuse_unreachable(&c);
            return 1;
        }
        if !super::fix_cover::fix(&mut c) {
            return 1;
        }
    }
    if c.cfg.search_multiple_owners {
        for (slot, owners) in super::owners::report(&mut c) {
            if !super::fix_owners::fix(&mut c, slot, &owners) {
                return 1;
            }
        }
    }
    0
}

fn refuse_unreachable(c: &Cluster) {
    let text = format!(
        "*** Fixing slots coverage with {} unreachable masters is dangerous: kevy-cli will assume that slots about masters that are not reachable are not covered, and will try to reassign them to the reachable nodes. This can cause data loss and is rarely what you want to do. If you really want to proceed use the --cluster-fix-with-unreachable-masters option.",
        c.unreachable_masters
    );
    log::line(c.cfg.color, Level::Err, text.as_bytes());
}
