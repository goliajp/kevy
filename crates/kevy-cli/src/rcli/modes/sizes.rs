//! How big each key of a page is, by type: one pipelined round trip for the
//! types, one for the sizes.

use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

/// What a size counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Measure {
    /// Length or cardinality, in the type's own unit (`--bigkeys`).
    Length,
    /// Bytes of memory, `MEMORY USAGE [SAMPLES n]` (`--memkeys`).
    Memory { samples: i64 },
}

/// One key type and what the walk found of it.
#[derive(Debug)]
pub(crate) struct Kind {
    pub(crate) name: Vec<u8>,
    /// The command that sizes a key of this type; `None` for types this
    /// client does not know (module types), which count with size 0.
    command: Option<&'static [u8]>,
    unit: &'static str,
    pub(crate) keys: u64,
    pub(crate) total: u64,
    /// The largest size seen and its key, quoted.
    pub(crate) biggest: Option<(u64, Vec<u8>)>,
}

impl Kind {
    fn new(name: &[u8], command: Option<&'static [u8]>, unit: &'static str) -> Kind {
        Kind { name: name.to_vec(), command, unit, keys: 0, total: 0, biggest: None }
    }

    /// The unit sizes of this kind are reported in.
    pub(crate) fn unit(&self, measure: Measure) -> &'static str {
        match measure {
            Measure::Length => self.unit,
            Measure::Memory { .. } => "bytes",
        }
    }
}

/// Every type seen so far: the built-in types first, in a fixed order
/// (DEV-020), then others in the order they turned up.
#[derive(Debug)]
pub(crate) struct Tally {
    pub(crate) kinds: Vec<Kind>,
}

impl Tally {
    pub(crate) fn new() -> Tally {
        let known: [(&[u8], &'static [u8], &'static str); 6] = [
            (b"string", b"STRLEN", "bytes"),
            (b"list", b"LLEN", "items"),
            (b"set", b"SCARD", "members"),
            (b"zset", b"ZCARD", "members"),
            (b"hash", b"HLEN", "fields"),
            (b"stream", b"XLEN", "entries"),
        ];
        Tally { kinds: known.iter().map(|(n, c, u)| Kind::new(n, Some(c), u)).collect() }
    }

    fn index(&mut self, name: &[u8]) -> usize {
        if let Some(i) = self.kinds.iter().position(|k| k.name == name) {
            return i;
        }
        self.kinds.push(Kind::new(name, None, "?"));
        self.kinds.len() - 1
    }
}

/// The type index and size of each key; `None` for a key gone before TYPE.
/// `Err` carries the message of a fatal failure.
pub(crate) fn measure(
    s: &mut Session,
    keys: &[Vec<u8>],
    tally: &mut Tally,
    measure: Measure,
) -> Result<Vec<Option<(usize, u64)>>, Vec<u8>> {
    let types = s
        .pipeline(&keys.iter().map(|k| vec![&b"TYPE"[..], k]).collect::<Vec<_>>())
        .map_err(|_| b"\nI/O error".to_vec())?;
    let mut kinds = Vec::with_capacity(keys.len());
    for reply in types {
        kinds.push(match reply {
            Reply::Error(msg) => {
                return Err([b"TYPE returned an error: ".as_slice(), &msg].concat());
            }
            Reply::Simple(name) | Reply::Bulk(name) if name != b"none" => Some(tally.index(&name)),
            _ => None,
        });
    }
    let sizes = sizes_of(s, keys, &kinds, tally, measure)?;
    Ok(kinds.iter().zip(sizes).map(|(k, size)| k.map(|i| (i, size))).collect())
}

/// One size per key, 0 where there is nothing to ask or the answer failed.
fn sizes_of(
    s: &mut Session,
    keys: &[Vec<u8>],
    kinds: &[Option<usize>],
    tally: &Tally,
    measure: Measure,
) -> Result<Vec<u64>, Vec<u8>> {
    let samples = match measure {
        Measure::Memory { samples } if samples > 0 => Some(samples.to_string()),
        _ => None,
    };
    let mut asked = Vec::new();
    let mut commands: Vec<Vec<&[u8]>> = Vec::new();
    for (i, (key, kind)) in keys.iter().zip(kinds).enumerate() {
        let Some(kind) = kind else { continue };
        let argv: Vec<&[u8]> = match (measure, tally.kinds[*kind].command, &samples) {
            (Measure::Memory { .. }, _, None) => vec![b"MEMORY", b"USAGE", key],
            (Measure::Memory { .. }, _, Some(n)) => {
                vec![b"MEMORY", b"USAGE", key, b"SAMPLES", n.as_bytes()]
            }
            (Measure::Length, Some(command), _) => vec![command, key],
            (Measure::Length, None, _) => continue,
        };
        asked.push(i);
        commands.push(argv);
    }
    let replies = s.pipeline(&commands).map_err(|_| b"\nI/O error".to_vec())?;
    let mut sizes = vec![0u64; keys.len()];
    for ((i, argv), reply) in asked.iter().zip(&commands).zip(replies) {
        match reply {
            Reply::Int(n) => sizes[*i] = n.max(0) as u64,
            _ => {
                let command: &[u8] = if argv[0] == b"MEMORY" { b"MEMORY USAGE" } else { argv[0] };
                let key = &keys[*i];
                eprint_bytes(&[
                    b"Warning:  ",
                    command,
                    b" on '",
                    key,
                    b"' failed (may have changed type)\n",
                ]);
            }
        }
    }
    Ok(sizes)
}
