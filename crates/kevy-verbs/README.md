# kevy-verbs

The command layer kevy's server and its embedded engine share: argv
parsing, reply encoding with Redis's error wording, and the execution
of each command against one `kevy_store::Store`.

Both faces call the same code, so a command cannot answer one way over
the network and another way in process. What stays with each face is
what really differs: locking, routing keys to shards, multi-key
coordination, and where the effects of a write are recorded.

This crate is infrastructure for `kevy` and `kevy-embedded`. Most
applications want one of those instead.
