//! kevy-vlog unit suite — round trip, CRC refusal, rotation, accounting,
//! pin semantics, compaction with a live owner, and a deterministic
//! randomized churn (splitmix64) that exercises all of it together.

use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use super::*;

fn dir(name: &str) -> kevy_tmpdir::TmpDir {
    kevy_tmpdir::TmpDir::new(name)
}

#[test]
fn round_trip_small_empty_and_large() {
    let d = dir("vlog-roundtrip");
    let mut v = Vlog::open(d.path(), DEFAULT_ROTATE_BYTES).unwrap();
    let cases: &[(&[u8], Vec<u8>)] = &[
        (b"k1", b"v1".to_vec()),
        (b"", Vec::new()),                    // empty key AND empty payload
        (b"k3", vec![0xAB; 1 << 20]),         // 1 MiB payload
        (b"k4\x00bin", vec![0u8; 3]),         // binary key
    ];
    let refs: Vec<VlogRef> =
        cases.iter().map(|(k, p)| v.append(k, p).unwrap()).collect();
    for ((k, p), r) in cases.iter().zip(&refs) {
        let (rk, rp) = v.read(*r).unwrap();
        assert_eq!((rk.as_slice(), &rp), (*k, p), "mismatch at {r:?}");
    }
}

#[test]
fn a_flipped_byte_is_refused_not_healed() {
    let d = dir("vlog-crc");
    let mut v = Vlog::open(d.path(), DEFAULT_ROTATE_BYTES).unwrap();
    let r = v.append(b"key", b"payload-payload").unwrap();
    // Flip one payload byte on disk behind the log's back.
    let pin = v.pin(r.file_id).unwrap();
    let mut b = [0u8; 1];
    pin.file.read_exact_at(&mut b, r.offset + 12).unwrap();
    pin.file.write_all_at(&[b[0] ^ 0xFF], r.offset + 12).unwrap();
    let err = v.read(r).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
    assert!(err.to_string().contains("crc"), "{err}");
}

#[test]
fn rotation_seals_files_and_old_refs_stay_readable() {
    let d = dir("vlog-rotate");
    let mut v = Vlog::open(d.path(), 64).unwrap(); // tiny threshold
    let refs: Vec<VlogRef> =
        (0..10).map(|i| v.append(format!("k{i}").as_bytes(), &[i as u8; 40]).unwrap()).collect();
    let ids: std::collections::BTreeSet<u32> = refs.iter().map(|r| r.file_id).collect();
    assert!(ids.len() >= 3, "64-byte rotation should have sealed several files: {ids:?}");
    assert_eq!(v.stats().files, ids.len().max(v.stats().files)); // all still present
    for (i, r) in refs.iter().enumerate() {
        let (_, p) = v.read(*r).unwrap();
        assert_eq!(p, vec![i as u8; 40]);
    }
}

#[test]
fn open_is_disposable_by_contract() {
    let d = dir("vlog-disposable");
    {
        let mut v = Vlog::open(d.path(), DEFAULT_ROTATE_BYTES).unwrap();
        v.append(b"stale", b"bytes").unwrap();
    } // plain drop: files stay on disk (process-exit shape)
    let survivors: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("vlog-"))
        .collect();
    assert!(!survivors.is_empty(), "plain drop must NOT delete (that's open's job)");
    let v = Vlog::open(d.path(), DEFAULT_ROTATE_BYTES).unwrap();
    assert_eq!(v.stats().bytes, 0, "open() must start empty (disposable)");
}

#[test]
fn mark_all_dead_lets_compaction_drop_sealed_files_without_a_scan() {
    let d = dir("vlog-mark-all-dead");
    let mut v = Vlog::open(d.path(), 64).unwrap(); // tiny threshold → several sealed files
    for i in 0..10 {
        v.append(format!("k{i}").as_bytes(), &[i as u8; 40]).unwrap();
    }
    assert!(v.stats().files >= 3);
    v.mark_all_dead();
    assert_eq!(v.stats().live_bytes, 0);
    // Owner that panics on any callback: full-dead files must drop scan-free.
    struct Nobody;
    impl CompactOwner for Nobody {
        fn is_live(&mut self, _: &[u8], _: VlogRef) -> bool {
            panic!("full-dead file must not be scanned")
        }
        fn moved(&mut self, _: &[u8], _: VlogRef, _: VlogRef) {
            panic!("nothing may move")
        }
    }
    let retired = v.compact_below(50, &mut Nobody).unwrap();
    assert!(retired >= 2, "sealed full-dead files must retire: {retired}");
    assert_eq!(v.stats().files, 1, "only the active file remains");
}

/// The store side of compaction, simulated: key -> live ref.
struct MapOwner {
    live: HashMap<Vec<u8>, VlogRef>,
    moves: usize,
}
impl CompactOwner for MapOwner {
    fn is_live(&mut self, key: &[u8], old: VlogRef) -> bool {
        self.live.get(key) == Some(&old)
    }
    fn moved(&mut self, key: &[u8], old: VlogRef, new: VlogRef) {
        assert_eq!(self.live.insert(key.to_vec(), new), Some(old));
        self.moves += 1;
    }
}

#[test]
fn compaction_moves_live_drops_dead_and_bumps_epoch() {
    let d = dir("vlog-compact");
    let mut v = Vlog::open(d.path(), 256).unwrap(); // rotate quickly
    let mut owner = MapOwner { live: HashMap::new(), moves: 0 };
    for i in 0..24 {
        let key = format!("k{i:02}");
        let r = v.append(key.as_bytes(), &[i as u8; 48]).unwrap();
        owner.live.insert(key.into_bytes(), r);
    }
    // Kill every odd key: note dead + forget the ref.
    for i in (1..24).step_by(2) {
        let key = format!("k{i:02}").into_bytes();
        let r = owner.live.remove(&key).unwrap();
        v.note_dead(r);
    }
    let (before_files, epoch0) = (v.stats().files, v.epoch());
    let retired = v.compact_below(60, &mut owner).unwrap();
    assert!(retired >= 1, "half-dead sealed files must compact");
    assert_eq!(v.epoch(), epoch0 + retired as u64, "one epoch bump per retired file");
    assert!(v.stats().files < before_files + retired, "retired files left the set");
    assert!(owner.moves >= 1, "live records must be re-homed");
    // Every surviving key reads back with its ORIGINAL bytes via the new ref.
    for (key, r) in &owner.live {
        let (rk, rp) = v.read(*r).unwrap();
        assert_eq!(&rk, key);
        let i: u8 = std::str::from_utf8(&key[1..]).unwrap().parse().unwrap();
        assert_eq!(rp, vec![i; 48], "payload corrupted across compaction for {key:?}");
    }
}

#[test]
fn fully_dead_files_are_dropped_without_a_scan() {
    let d = dir("vlog-alldead");
    let mut v = Vlog::open(d.path(), 128).unwrap();
    let mut refs = Vec::new();
    for i in 0..8 {
        refs.push(v.append(format!("k{i}").as_bytes(), &[0xEE; 64]).unwrap());
    }
    for r in &refs {
        v.note_dead(*r);
    }
    // An owner that would PANIC if consulted — proves the no-scan path.
    struct NoCalls;
    impl CompactOwner for NoCalls {
        fn is_live(&mut self, _: &[u8], _: VlogRef) -> bool {
            panic!("fully-dead file must not consult the owner")
        }
        fn moved(&mut self, _: &[u8], _: VlogRef, _: VlogRef) {
            panic!("nothing to move")
        }
    }
    let retired = v.compact_below(60, &mut NoCalls).unwrap();
    assert!(retired >= 1);
}

#[test]
fn a_pin_keeps_the_file_on_disk_until_dropped() {
    let d = dir("vlog-pin");
    let mut v = Vlog::open(d.path(), 64).unwrap();
    // Incompressible payloads: sealing is driven by ON-DISK bytes, and
    // a constant run would compress far below the tiny threshold.
    let noise: Vec<u8> = (0..50u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
    let r = v.append(b"pinned", &noise).unwrap();
    v.append(b"filler", &noise).unwrap(); // seals file 0
    let pin: Arc<VlogFile> = v.pin(r.file_id).unwrap();
    let path = d.path().join(format!("vlog-{:08}.dat", r.file_id));

    v.note_dead(r); // make file 0 fully dead, then retire it
    let mut owner = MapOwner { live: HashMap::new(), moves: 0 };
    assert!(v.compact_below(60, &mut owner).unwrap() >= 1);

    assert!(path.exists(), "pinned file must survive its own retirement");
    let (_, p) = pin.read(r).unwrap();
    assert_eq!(p, noise, "pinned reader still sees its record");
    drop(pin);
    assert!(!path.exists(), "last pin drop unlinks the retired file");
}

#[test]
fn stats_track_bytes_and_live() {
    let d = dir("vlog-stats");
    let mut v = Vlog::open(d.path(), DEFAULT_ROTATE_BYTES).unwrap();
    assert_eq!(v.stats().bytes, 0);
    let r1 = v.append(b"a", &[1; 10]).unwrap();
    let _r2 = v.append(b"b", &[2; 10]).unwrap();
    let s = v.stats();
    assert_eq!(s.bytes, s.live_bytes, "everything starts live");
    assert!(s.bytes > 0);
    v.note_dead(r1);
    let s2 = v.stats();
    assert_eq!(s2.bytes, s.bytes, "note_dead never shrinks the file");
    assert!(s2.live_bytes < s2.bytes, "dead bytes left the live count");
}

/// Deterministic splitmix64 — the house randomized-property style.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test]
fn randomized_churn_survives_rotations_and_compactions() {
    let d = dir("vlog-churn");
    let mut v = Vlog::open(d.path(), 512).unwrap();
    let mut owner = MapOwner { live: HashMap::new(), moves: 0 };
    let mut rng = 0xC0FFEE_u64;
    for step in 0..2_000u64 {
        match splitmix(&mut rng) % 10 {
            // 60%: append (overwrite = old ref dies)
            0..=5 => {
                let k = format!("key{}", splitmix(&mut rng) % 200).into_bytes();
                let plen = (splitmix(&mut rng) % 300) as usize;
                let fill = (step % 251) as u8;
                let r = v.append(&k, &vec![fill; plen]).unwrap();
                if let Some(old) = owner.live.insert(k, r) {
                    v.note_dead(old);
                }
            }
            // 20%: delete
            6..=7 => {
                let k = format!("key{}", splitmix(&mut rng) % 200).into_bytes();
                if let Some(old) = owner.live.remove(&k) {
                    v.note_dead(old);
                }
            }
            // 10%: read a random live key
            8 => {
                if let Some((k, r)) = owner.live.iter().next() {
                    let (rk, _) = v.read(*r).unwrap();
                    assert_eq!(&rk, k);
                }
            }
            // 10%: compact
            _ => {
                v.compact_below(50, &mut owner).unwrap();
            }
        }
    }
    v.compact_below(101, &mut owner).unwrap(); // force-compact all sealed files
    // Every live key must read back exactly; stats must be coherent.
    for (k, r) in &owner.live {
        let (rk, _) = v.read(*r).unwrap();
        assert_eq!(&rk, k, "live key lost after churn");
    }
    let s = v.stats();
    assert!(s.live_bytes <= s.bytes);
    assert!(s.epoch > 0, "churn at 10% compact rate must have retired files");
}

#[test]
fn image_fetch_and_verify_round_trip() {
    // The batched-read split: `read_image` (issuance) + `verify_image`
    // (completion) must equal the one-call `read`.
    let d = dir("vlog-image");
    let mut v = Vlog::open(d.path(), DEFAULT_ROTATE_BYTES).unwrap();
    let r = v.append(b"row:1", b"payload-bytes").unwrap();
    let pin = v.pin(r.file_id).unwrap();
    assert_eq!(r.disk_len(), 8 + r.len as usize);
    let image = pin.read_image(r).unwrap();
    assert_eq!(image.len(), r.disk_len());
    // `verify_image` yields the stored FRAME; decoding against the
    // file's dictionary is the separate completion step.
    let (k, frame) = verify_image(r, image).unwrap();
    let p = pin.decompress(&frame).unwrap();
    assert_eq!((k.as_slice(), p.as_slice()), (&b"row:1"[..], &b"payload-bytes"[..]));
    assert_eq!(pin.read(r).unwrap(), (b"row:1".to_vec(), b"payload-bytes".to_vec()));
}

#[test]
fn verify_image_refuses_wrong_length_and_flipped_bytes() {
    let d = dir("vlog-image-bad");
    let mut v = Vlog::open(d.path(), DEFAULT_ROTATE_BYTES).unwrap();
    let r = v.append(b"key", b"payload").unwrap();
    let pin = v.pin(r.file_id).unwrap();
    // Truncated image → length mismatch, refused.
    let mut short = pin.read_image(r).unwrap();
    short.pop();
    assert!(verify_image(r, short).is_err());
    // Flipped payload byte → CRC mismatch, refused (not healed).
    let mut flipped = pin.read_image(r).unwrap();
    let last = flipped.len() - 1;
    flipped[last] ^= 0xFF;
    assert!(verify_image(r, flipped).is_err());
}

#[test]
fn rotation_trains_a_dictionary_and_records_shrink_against_it() {
    let d = dir("vlog-dict");
    // Small threshold so the second batch of appends lands in file 1,
    // whose dictionary was trained on file 0's samples.
    let mut v = Vlog::open(d.path(), 4096).unwrap();
    let value: Vec<u8> = (0..400u32).map(|i| (i.wrapping_mul(7) % 251) as u8).collect();
    let mut refs = Vec::new();
    for i in 0..60 {
        refs.push(v.append(format!("k{i}").as_bytes(), &value).unwrap());
    }
    let first = refs[0];
    let last = *refs.last().unwrap();
    assert!(last.file_id > first.file_id, "the run must cross a rotation");
    // Same 400 B value: in the dictionary-bearing file it collapses to
    // a frame a fraction of the raw size (the K4 shape, end to end
    // through the vlog), and still reads back identical.
    assert!(
        (last.len as usize) < value.len() / 4,
        "post-rotation record should collapse against the trained dictionary \
         (got {} B for a {} B value)",
        last.len,
        value.len()
    );
    for r in &refs {
        let (_, p) = v.read(*r).unwrap();
        assert_eq!(p, value);
    }
    // The stored-bytes identity: stats.bytes is exactly the sum of every
    // record's on-disk length — frames included, nothing hidden.
    let s = v.stats();
    let sum: u64 = refs.iter().map(|r| r.disk_len() as u64).sum();
    assert_eq!(s.bytes, sum, "stored == sum(header + frame body), exact");
}
