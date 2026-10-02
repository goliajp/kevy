//! `COMMAND GETKEYS` / `GETKEYSANDFLAGS` / `INFO` against the replies
//! Redis 8.10.2 gave to the same commands, in both protocols. Each line is
//! the protocol, the argv and the reply; a byte is escaped as `\xNN`,
//! `\r`, `\n`, `\t` or `\\`.

use kevy_resp::{Argv, RespVersion};

fn unescape(s: &str) -> Vec<u8> {
    let (mut out, b) = (Vec::new(), s.as_bytes());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let (byte, width) = match b[i + 1] {
            b'r' => Some((b'\r', 2)),
            b'n' => Some((b'\n', 2)),
            b't' => Some((b'\t', 2)),
            b'\\' => Some((b'\\', 2)),
            b'x' => u8::from_str_radix(&s[i + 2..i + 4], 16).ok().map(|b| (b, 4)),
            _ => None,
        }
        .expect("a known escape");
        out.push(byte);
        i += width;
    }
    out
}

#[test]
fn command_keys_and_info_match_redis() {
    let mut checked = 0;
    for line in include_str!("testdata/redis_command_keys.txt").lines() {
        let (proto, rest) = line.split_once('\t').expect("a protocol");
        let (cmd, want) = rest.split_once("\t=>\t").expect("a command, then its reply");
        let argv = Argv::from(cmd.split('\t').map(unescape).collect::<Vec<_>>());
        let proto = if proto == "3" { RespVersion::V3 } else { RespVersion::V2 };
        let mut out = Vec::new();
        crate::cmd_command::cmd_command(&argv, &mut out, proto);
        assert_eq!(
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&unescape(want)),
            "{cmd} under RESP{proto:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, 1228);
}
