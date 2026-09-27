use kevy_resp::Argv;
use kevy_store::Store;

use crate::{Effect, exec};

fn argv(cmd: &str) -> Argv {
    Argv::from(cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>())
}

fn run(store: &mut Store, cmd: &str) -> (Option<Effect>, String) {
    let a = argv(cmd);
    let mut buf = [0u8; 32];
    let up = crate::args::upper_verb(&a[0], &mut buf).to_vec();
    let mut out = Vec::new();
    let e = exec(store, &up, &a, &mut out);
    (e, String::from_utf8_lossy(&out).into_owned())
}

fn frame(f: &[Vec<u8>]) -> String {
    f.iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join(" ")
}

/// Every frame the effect records, as text; the argv itself for a write.
fn records(cmd: &str, e: Option<Effect>) -> Vec<String> {
    match e {
        Some(Effect::Write) => vec![cmd.to_string()],
        Some(Effect::Record(f)) => vec![frame(&f)],
        Some(Effect::RecordAll(fs)) => fs.iter().map(|f| frame(f)).collect(),
        _ => Vec::new(),
    }
}

#[test]
fn a_generated_id_is_recorded_as_the_id_it_gave() {
    let mut s = Store::new();
    let (e, reply) = run(&mut s, "XADD s NOMKSTREAM MAXLEN ~ 2 * f v");
    assert_eq!(e, Some(Effect::Unchanged), "{reply}");
    let (e, reply) = run(&mut s, "XADD s MAXLEN ~ 2 * f v");
    let id = reply.split("\r\n").nth(1).unwrap().to_string();
    assert_eq!(records("", e), vec![format!("XADD s MAXLEN ~ 2 {id} f v")]);
    let (e, _) = run(&mut s, "XADD s2 7-* f v");
    assert_eq!(records("", e), vec!["XADD s2 7-0 f v".to_string()]);
    let (e, _) = run(&mut s, "XADD s2 8-1 f v");
    assert_eq!(e, Some(Effect::Write), "an explicit ID is recorded as typed");
    let (e, reply) = run(&mut s, "XADD s2 1-* f v");
    assert!(reply.starts_with("-ERR The ID"), "{reply}");
    assert_eq!(e, Some(Effect::Write), "a refusal changes nothing, however it is recorded");
}

/// Replaying the records onto the state the commands started from gives
/// what the commands gave, whatever the clock says by then.
#[test]
fn claim_records_replay_to_the_same_pending_list() {
    let setup = ["XADD s 1-1 a 1", "XADD s 2-1 b 2", "XADD s 3-1 c 3", "XGROUP CREATE s g 0"];
    let mut live = Store::new();
    let mut log: Vec<String> = Vec::new();
    for c in setup.iter().chain(&["XREADGROUP GROUP g a STREAMS s >"]) {
        let (e, _) = run(&mut live, c);
        log.extend(records(c, e));
    }
    let (_, gone) = run(&mut live, "XDEL s 3-1");
    assert_eq!(gone, ":1\r\n");
    log.push("XDEL s 3-1".into());
    // 4-1 was never delivered: only FORCE puts it in the list
    let (e, _) = run(&mut live, "XADD s 4-1 d 4");
    log.extend(records("XADD s 4-1 d 4", e));
    for c in [
        "XCLAIM s g b 0 1-1 2-1 RETRYCOUNT 7",
        "XAUTOCLAIM s g c 0 2-1 COUNT 5 JUSTID",
        "XCLAIM s g e 0 4-1 FORCE JUSTID",
    ] {
        let (e, _) = run(&mut live, c);
        let rec = records(c, e);
        assert!(!rec.is_empty() && rec.iter().all(|f| f.starts_with("XCLAIM s g ")), "{rec:?}");
        log.extend(rec);
    }
    // a replay runs later than the commands did
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut replayed = Store::new();
    for f in &log {
        run(&mut replayed, f);
    }
    let read = "XPENDING s g - + 10";
    let blank = |s: String| {
        let mut t: Vec<String> = s.split("\r\n").map(str::to_string).collect();
        for i in 0..t.len() {
            if t[i] == "*4" && i + 5 < t.len() {
                t[i + 5] = ":idle".into();
            }
        }
        t.join("\r\n")
    };
    assert_eq!(blank(run(&mut replayed, read).1), blank(run(&mut live, read).1), "{log:#?}");
    // the idle column hides the delivery times; the rows hold them
    assert_eq!(pel(&mut replayed), pel(&mut live));
    let pending = run(&mut live, read).1;
    assert!(pending.contains("$1\r\nb\r\n") && pending.contains("$1\r\nc\r\n"), "{pending}");
    assert!(pending.contains(":7\r\n"), "RETRYCOUNT survives: {pending}");
    assert!(pending.contains("$1\r\ne\r\n"), "FORCE created a row: {pending}");
}

/// `(id, owner, delivery time, delivery count)` of group `g` on `s`.
fn pel(store: &mut Store) -> Vec<(String, Vec<u8>, u64, u32)> {
    let s = store.stream_view(b"s").unwrap().unwrap();
    let g = s.group(b"g").unwrap();
    g.pel
        .iter()
        .map(|(id, p)| {
            let owner = p.consumer.as_slice().to_vec();
            (String::from_utf8(id.encode()).unwrap(), owner, p.delivery_time_ms, p.delivery_count)
        })
        .collect()
}

#[test]
fn a_claim_that_changes_nothing_records_nothing() {
    let mut s = Store::new();
    for c in ["XADD s 1-1 a 1", "XGROUP CREATE s g 0", "XREADGROUP GROUP g a STREAMS s >"] {
        run(&mut s, c);
    }
    // the consumer is new: that much changed
    let (e, _) = run(&mut s, "XAUTOCLAIM s g idle 999999 0");
    assert_eq!(records("", e), vec!["XGROUP CREATECONSUMER s g idle".to_string()]);
    let (e, _) = run(&mut s, "XAUTOCLAIM s g idle 999999 0");
    assert_eq!(e, Some(Effect::Skip));
    let (e, _) = run(&mut s, "XCLAIM s g a 999999 1-1");
    assert_eq!(e, Some(Effect::Skip));
    let (e, _) = run(&mut s, "XCLAIM s g a 0 9-9");
    assert_eq!(e, Some(Effect::Skip), "an ID nobody holds");
}

#[test]
fn a_dropped_entry_is_recorded_as_a_drop() {
    let mut s = Store::new();
    for c in ["XADD s 1-1 a 1", "XADD s 2-1 b 2", "XGROUP CREATE s g 0"] {
        run(&mut s, c);
    }
    run(&mut s, "XREADGROUP GROUP g a STREAMS s >");
    run(&mut s, "XDEL s 1-1");
    let (e, reply) = run(&mut s, "XCLAIM s g a 0 1-1 JUSTID");
    assert_eq!(reply, "*0\r\n");
    assert_eq!(records("", e), vec!["XCLAIM s g a 0 1-1 JUSTID".to_string()]);
}
