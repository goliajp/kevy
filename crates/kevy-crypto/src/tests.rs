//! Known-answer tests against the published vectors in `tests/data/`.
//! Every suite counts what it ran against the count its fixture declares,
//! so a parser that silently matches nothing cannot pass.

use crate::{aead, blake2s, chacha20, hkdf, poly1305, x25519};

fn hex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    assert!(s.len().is_multiple_of(2), "odd hex: {s}");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn arr<const N: usize>(s: &str) -> [u8; N] {
    hex(s).try_into().unwrap()
}

fn records(fixture: &str) -> impl Iterator<Item = Vec<&str>> {
    fixture.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(|l| l.split(' ').collect())
}

/// The number the fixture's header states for its case count.
fn declared(fixture: &str) -> usize {
    let line = fixture.lines().nth(2).unwrap();
    line.trim_start_matches("# ").split(' ').next().unwrap().parse().unwrap()
}

const RFC8439: &str = include_str!("../tests/data/rfc8439_appendix_a.txt");

/// section/vector -> label -> value, from the RFC 8439 Appendix A fixture.
fn rfc8439(section: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    let mut out: Vec<(String, std::collections::BTreeMap<String, String>)> = Vec::new();
    for r in records(RFC8439).filter(|r| r[0] == section) {
        if out.last().map(|(v, _)| v.as_str()) != Some(r[1]) {
            out.push((r[1].to_string(), Default::default()));
        }
        out.last_mut().unwrap().1.insert(r[2].to_string(), r[3].to_string());
    }
    out.into_iter().map(|(_, m)| m).collect()
}

#[test]
fn rfc8439_a1_block_function() {
    let vs = rfc8439("A1");
    assert_eq!(vs.len(), 5);
    for v in vs {
        let ks = chacha20::block(&arr(&v["Key"]), v["counter"].parse().unwrap(), &arr(&v["Nonce"]));
        assert_eq!(ks.to_vec(), hex(&v["Keystream"]));
    }
}

#[test]
fn rfc8439_a2_encryption() {
    let vs = rfc8439("A2");
    assert_eq!(vs.len(), 3);
    for v in vs {
        let mut buf = hex(&v["Plaintext"]);
        chacha20::xor_keystream(
            &arr(&v["Key"]),
            v["counter"].parse().unwrap(),
            &arr(&v["Nonce"]),
            &mut buf,
        );
        assert_eq!(buf, hex(&v["Ciphertext"]));
    }
}

#[test]
fn rfc8439_a3_poly1305() {
    let vs = rfc8439("A3");
    assert_eq!(vs.len(), 11);
    for v in vs {
        let (key, msg, tag) = match v.get("One-time_Poly1305_Key") {
            Some(k) => (hex(k), hex(&v["Text_to_MAC"]), hex(&v["Tag"])),
            None => ([hex(&v["R"]), hex(&v["S"])].concat(), hex(&v["data"]), hex(&v["tag"])),
        };
        assert_eq!(poly1305::mac(&key.clone().try_into().unwrap(), &msg).to_vec(), tag);
        // the same MAC fed in uneven pieces
        let mut p = poly1305::Poly1305::new(&key.try_into().unwrap());
        for piece in msg.chunks(7) {
            p.update(piece);
        }
        assert_eq!(p.finalize().to_vec(), tag);
    }
}

#[test]
fn rfc8439_a4_one_time_key() {
    let vs = rfc8439("A4");
    assert_eq!(vs.len(), 3);
    for v in vs {
        let block = chacha20::block(&arr(&v["The_ChaCha20_Key"]), 0, &arr(&v["The_nonce"]));
        assert_eq!(block[..32].to_vec(), hex(&v["Poly1305_one-time_key"]));
    }
}

#[test]
fn rfc8439_a5_aead_decryption() {
    let vs = rfc8439("A5");
    assert_eq!(vs.len(), 1);
    let v = &vs[0];
    let (key, nonce, aad) = (arr(&v["The_ChaCha20_Key"]), arr(&v["The_nonce"]), hex(&v["The_AAD"]));
    let mut buf = hex(&v["Ciphertext"]);
    aead::open(&key, &nonce, &aad, &mut buf, &arr(&v["Received_Tag"])).unwrap();
    assert_eq!(buf, hex(&v["Plaintext"]));
    assert_eq!(aead::seal(&key, &nonce, &aad, &mut buf).to_vec(), hex(&v["Calculated_Tag"]));
}

#[test]
fn wycheproof_chacha20_poly1305() {
    let fx = include_str!("../tests/data/wycheproof_chacha20_poly1305.txt");
    let mut ran = 0;
    for r in records(fx) {
        let (key, nonce, aad, msg, ct, tag) =
            (arr(r[1]), arr(r[2]), hex(r[3]), hex(r[4]), hex(r[5]), hex(r[6]));
        let valid = r[7] == "1";
        let mut buf = ct.clone();
        let tag16: [u8; 16] = tag.clone().try_into().unwrap();
        let opened = aead::open(&key, &nonce, &aad, &mut buf, &tag16);
        assert_eq!(opened.is_ok(), valid, "tcId {}", r[0]);
        if valid {
            assert_eq!(buf, msg, "tcId {}", r[0]);
            let mut sealed = msg.clone();
            assert_eq!(aead::seal(&key, &nonce, &aad, &mut sealed).to_vec(), tag, "tcId {}", r[0]);
            assert_eq!(sealed, ct, "tcId {}", r[0]);
        } else {
            assert_eq!(buf, ct, "tcId {} decrypted despite a bad tag", r[0]);
        }
        ran += 1;
    }
    assert_eq!(ran, declared(fx));
}

#[test]
fn rfc7748_x25519() {
    let fx = include_str!("../tests/data/rfc7748_x25519.txt");
    let mut seen = 0;
    for r in records(fx) {
        match r[0] {
            "scalarmult" => assert_eq!(x25519::x25519(&arr(r[1]), &arr(r[2])), arr::<32>(r[3])),
            "iterate" => {
                let (mut k, mut u) = (x25519::BASEPOINT, x25519::BASEPOINT);
                for _ in 0..r[1].parse::<u32>().unwrap() {
                    let next = x25519::x25519(&k, &u);
                    u = k;
                    k = next;
                }
                assert_eq!(k, arr::<32>(r[2]), "after {} iterations", r[1]);
            }
            "dh" => {
                let [a, pa, b, pb, shared] = [1, 2, 3, 4, 5].map(|i| arr::<32>(r[i]));
                assert_eq!(x25519::x25519(&a, &x25519::BASEPOINT), pa);
                assert_eq!(x25519::x25519(&b, &x25519::BASEPOINT), pb);
                assert_eq!(x25519::x25519(&a, &pb), shared);
                assert_eq!(x25519::x25519(&b, &pa), shared);
            }
            other => panic!("unknown record {other}"),
        }
        seen += 1;
    }
    assert_eq!(seen, 5);
}

#[test]
fn wycheproof_x25519() {
    let fx = include_str!("../tests/data/wycheproof_x25519.txt");
    let mut ran = 0;
    for r in records(fx) {
        assert_eq!(
            x25519::x25519(&arr(r[2]), &arr(r[1])),
            arr::<32>(r[3]),
            "tcId {} ({})",
            r[0],
            r[4]
        );
        ran += 1;
    }
    assert_eq!(ran, declared(fx));
}

#[test]
fn blake2s_kat_keyed() {
    let fx = include_str!("../tests/data/blake2s_kat.txt");
    let mut ran = 0;
    for r in records(fx) {
        let (input, key, want) = (hex(r[0]), hex(r[1]), hex(r[2]));
        let mut h = blake2s::Blake2s::new_keyed(32, &key);
        // uneven pieces exercise the held-back last block
        for piece in input.chunks(13) {
            h.update(piece);
        }
        let mut out = [0u8; 32];
        h.finalize(&mut out);
        assert_eq!(out.to_vec(), want, "input length {}", input.len());
        ran += 1;
    }
    assert_eq!(ran, declared(fx));
}

fn selftest_seq(len: usize, seed: u32) -> Vec<u8> {
    let (mut a, mut b) = (0xDEAD_4BADu32.wrapping_mul(seed), 1u32);
    (0..len)
        .map(|_| {
            let t = a.wrapping_add(b);
            a = b;
            b = t;
            (t >> 24) as u8
        })
        .collect()
}

#[test]
fn rfc7693_abc_and_self_test() {
    let fx = include_str!("../tests/data/rfc7693_blake2s.txt");
    let field = |name: &str| records(fx).find(|r| r[0] == name).unwrap()[1].to_string();
    assert_eq!(blake2s::hash(b"abc").to_vec(), hex(&field("abc")));
    assert_eq!(field("selftest_seq_multiplier"), "0xDEAD4BAD");
    let md_lens: Vec<usize> =
        field("selftest_md_len").split(',').map(|x| x.parse().unwrap()).collect();
    let in_lens: Vec<usize> =
        field("selftest_in_len").split(',').map(|x| x.parse().unwrap()).collect();
    let mut grand = blake2s::Blake2s::new(32);
    for &outlen in &md_lens {
        for &inlen in &in_lens {
            let input = selftest_seq(inlen, inlen as u32);
            let mut md = vec![0u8; outlen];
            let mut h = blake2s::Blake2s::new(outlen);
            h.update(&input);
            h.finalize(&mut md);
            grand.update(&md);
            let key = selftest_seq(outlen, outlen as u32);
            let mut h = blake2s::Blake2s::new_keyed(outlen, &key);
            h.update(&input);
            h.finalize(&mut md);
            grand.update(&md);
        }
    }
    let mut out = [0u8; 32];
    grand.finalize(&mut out);
    assert_eq!(out.to_vec(), hex(&field("selftest_grand_hash")));
}

#[test]
fn hmac_is_rfc2104_over_blake2s() {
    // the definition, spelled out once without the helper
    let key = [0x0bu8; 32];
    let msg = b"Hi There";
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..32 {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let inner = blake2s::hash(&[&ipad[..], msg].concat());
    let want = blake2s::hash(&[&opad[..], &inner].concat());
    assert_eq!(hkdf::hmac(&key, &[msg]), want);
}

#[test]
fn ct_eq_depends_on_content_and_length() {
    assert!(crate::ct_eq(b"", b""));
    assert!(!crate::ct_eq(b"a", b""));
    assert!(!crate::ct_eq(&[0; 16], &[[0; 15].as_slice(), &[1]].concat()));
}

#[test]
#[should_panic(expected = "out_len 1..=32")]
fn blake2s_refuses_a_zero_length_digest() {
    let _ = blake2s::Blake2s::new(0);
}

#[test]
#[should_panic(expected = "output buffer length")]
fn blake2s_refuses_a_mismatched_output_buffer() {
    blake2s::Blake2s::new(32).finalize(&mut [0u8; 16]);
}

#[test]
#[should_panic(expected = "longer than one block")]
fn hmac_refuses_a_key_longer_than_a_block() {
    let _ = hkdf::hmac(&[0u8; 65], &[b""]);
}

#[test]
fn blake2s_debug_prints_the_digest_length_and_never_the_key_state() {
    let keyed = blake2s::Blake2s::new_keyed(16, b"secret");
    assert_eq!(format!("{keyed:?}"), "Blake2s { out_len: 16, .. }");
    assert_eq!(format!("{:?}", blake2s::Blake2s::new(32)), "Blake2s { out_len: 32, .. }");
}

#[test]
fn auth_error_says_what_failed() {
    assert_eq!(aead::AuthError.to_string(), "authentication tag mismatch");
}
