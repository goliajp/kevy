# Changelog

Earlier releases are described in the repository's
[CHANGELOG.md](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md).

## 7.0.0

Installs the kevy 7.0.0 server and `kevy-cli`. No change to the launcher;
the platform packages carry the 7.0.0 binaries. The full list of changes is
in the repository changelog. Read
[the upgrade guide](https://github.com/goliajp/kevy/blob/develop/docs/upgrading-6.4-to-7.0.md)
before upgrading if you:

- set `maxmemory`: `used_memory` reads about 1.5 times higher for the same
  data;
- run replicas: upgrade the primary first;
- may go back to 6.4: a data directory 7.0 has written opens in 6.4 without
  its indexes, views and tables;
- script `kevy-cli` tools as bare words (`kevy-cli doctor`,
  `kevy-cli export`, …): in 7.0 a bare word is a server command, and the
  tools answer behind `--kevy`.

A wire client needs no code change, and a 6.4 data directory opens as it
is.
