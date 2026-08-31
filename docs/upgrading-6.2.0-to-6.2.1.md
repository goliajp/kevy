# Upgrading from 6.2.0 to 6.2.1

The short version: **nothing on the server changes.** 6.2.1 exists
because two client crates — `kevy-client` and `kevy-client-async` — sat
on crates.io at 2.2.0, built against the 5.x engine, while every kevy
release since 6.0.0 described them as 6.x. If you run the server, use a
binding, or embed the engine from Rust, there is nothing to do. If you
depend on either client crate from crates.io, change one line.

## Why this release exists

- **Two crates were published without ever being sent.** `kevy-client`
  and `kevy-client-async` kept a version line of their own (2.x). When
  the workspace moved to 6.0.0, their manifests in the tree pinned every
  sibling at 6.x, but their own number stayed at 2.2.0 — nothing about
  their API had changed. `cargo publish` refuses a version that already
  exists, and the release workflow read that refusal as "already done".
  So 6.0.0, 6.1.0 and 6.2.0 each shipped 40 crates and skipped these two,
  and `cargo add kevy-client` kept resolving to a client that pulls
  `kevy-embedded 5.4.1`. A user noticed; the gates had not.

- **Every crate now carries the workspace version.** The two clients, and
  three other crates that declared the workspace number by hand rather
  than inheriting it, are on `version.workspace = true`. A bump moves all
  of them, and the version gate refuses a workspace member that declares
  a version of its own. The publish loop no longer takes "already exists"
  at its word: it compares the manifest crates.io holds with the one the
  tree would upload, and fails the release when they differ.

## What carries over unchanged

- **The wire.** RESP2 and RESP3 replies are byte-identical to 6.2.0. No
  client — Redis-generic or kevy's own — needs anything for the server.
- **The data directory.** 6.2.1 is 6.2.0's code under a new number; AOF,
  snapshots, the value log and every checkpoint open exactly as before,
  in both directions.
- **Every other crate and binding.** They move from 6.2.0 to 6.2.1 with
  no code change. The bump is there so that one number names one release
  across all of them.
- **The client API.** `kevy-client 6.2.1` is `kevy-client 2.2.0`'s API
  with its dependencies pointed at the engine this changelog describes.
  No call changes.

## If you depend on kevy-client or kevy-client-async

Change the requirement to the engine's major:

```toml
[dependencies]
kevy-client = "6"                                              # was "2"
kevy-client-async = { version = "6", features = ["tokio"] }   # was "2"
```

A requirement written as `"2"` keeps resolving to 2.2.0, which pins
`kevy-embedded ^5.0` and therefore compiles a 5.4.1 engine into your
binary. If your crate also depends on `kevy-resp 6` or `kevy-embedded 6`
directly, that gives you two copies of each and type errors at the
seams — the reason the report that started this reached us.

## Recommended procedure

Edit the requirement, then let cargo prove the graph has one engine in
it:

```sh
cargo update -p kevy-client -p kevy-client-async
cargo tree -i kevy-embedded      # exactly one line, at 6.2.1
```

If the second command prints two versions, something else in your
graph still asks for a 5.x engine; `cargo tree -i kevy-embedded@5.4.1`
names it.
