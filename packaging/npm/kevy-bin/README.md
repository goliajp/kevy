# @goliapkg/kevy-bin

The [kevy](https://github.com/goliajp/kevy) server and `kevy-cli`, as
prebuilt native binaries installed through npm, yarn, pnpm or bun.

```sh
npm install -g @goliapkg/kevy-bin

kevy --port 6004            # start a server on 127.0.0.1:6004
kevy-cli -p 6004 PING       # PONG
```

The binaries come from one platform package, which the package manager
picks as an optional dependency:

| Platform | Package |
|---|---|
| Linux x86-64 | `@goliapkg/kevy-bin-linux-x64` |
| Linux arm64 | `@goliapkg/kevy-bin-linux-arm64` |
| macOS arm64 | `@goliapkg/kevy-bin-darwin-arm64` |

Nothing runs at install time and nothing is downloaded afterwards; the
`kevy` and `kevy-cli` commands exec the binary from that package. On
another platform, or when optional dependencies were skipped
(`--omit=optional`), the commands say so and exit. Build from source there
with `cargo install kevy kevy-cli`.

The same binaries, each with a SHA-256 file, are on the
[GitHub releases](https://github.com/goliajp/kevy/releases) page.

- [kevy README](https://github.com/goliajp/kevy#readme)
- [Upgrading from 6.4 to 7.0](https://github.com/goliajp/kevy/blob/develop/docs/upgrading-6.4-to-7.0.md)
- [Changelog](https://github.com/goliajp/kevy/blob/develop/packaging/npm/kevy-bin/CHANGELOG.md)
