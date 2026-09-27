//! A slot several masters own or hold keys in: give it to the one with most
//! keys and move the others' keys there.

use super::log::{self, Level};
use super::topology::Cluster;

/// `false` when it could not.
pub(crate) fn fix(c: &mut Cluster, slot: u16, owners: &[usize]) -> bool {
    let color = c.cfg.color;
    log::line(
        color,
        Level::Info,
        format!(">>> Fixing multiple owners for slot {slot}...").as_bytes(),
    );
    let Some(owner) = super::owner::most_keys(c, owners, slot) else { return false };
    let text =
        [format!(">>> Setting slot {slot} owner: ").as_bytes(), &c.nodes[owner].shown()].concat();
    log::line(color, Level::Info, &text);
    let moved = match super::owner::set(c, owner, slot) {
        Ok(()) => owners.iter().filter(|&&o| o != owner).all(|&o| {
            super::owner::keys_in(c, o, slot) == 0
                || super::migrate_slot::move_keys_only(c, (o, owner), slot)
        }),
        Err(why) => {
            super::migrate::node_error(c, owner, &why);
            false
        }
    };
    if !moved {
        let text = format!("Failed to fix multiple owners for slot {slot}");
        log::line(color, Level::Err, text.as_bytes());
    }
    moved
}
