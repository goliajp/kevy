// RespVersion — the per-connection RESP protocol version (client-contract
// §4.1). V2 is the default; V3 (negotiated by HELLO 3) adds the nine extra
// prefixes plus out-of-band push frames.
package jp.golia.kevy;

public enum RespVersion {
    V2, V3
}
