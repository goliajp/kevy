// URL scheme routing + rejections (client-contract §1.1). Pure — no I/O.
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;

class UrlTest {
    @Test void memAnonAndNamed() {
        assertInstanceOf(Url.MemAnon.class, Url.parse("mem://"));
        Url.MemNamed n = assertInstanceOf(Url.MemNamed.class, Url.parse("mem://bus"));
        assertEquals("bus", n.name());
        assertEquals("mem://bus", n.registryKey());
    }

    @Test void fileNeedsPath() {
        assertInstanceOf(Url.FileStore.class, Url.parse("file:///tmp/kevy-x"));
        assertThrows(InvalidInputException.class, () -> Url.parse("file://"));
    }

    @Test void remoteDefaultsAndDb() {
        Url.Remote r = assertInstanceOf(Url.Remote.class, Url.parse("kevy://h"));
        assertEquals("h", r.host());
        assertEquals(6379, r.port());
        assertNull(r.db());
        Url.Remote r2 = assertInstanceOf(Url.Remote.class, Url.parse("redis://h:7000/3"));
        assertEquals(7000, r2.port());
        assertEquals(3, r2.db());
    }

    @Test void tcpIsRawIgnoresDb() {
        Url.Remote r = assertInstanceOf(Url.Remote.class, Url.parse("tcp://h:6400/5"));
        assertTrue(r.raw());
        assertNull(r.db(), "tcp:// is raw and never SELECTs");
    }

    @Test void tlsAndAuthUnsupported() {
        assertThrows(UnsupportedException.class, () -> Url.parse("rediss://h"));
        assertThrows(UnsupportedException.class, () -> Url.parse("kevys://h"));
        assertThrows(UnsupportedException.class, () -> Url.parse("redis://user:pass@h"));
    }

    @Test void unknownSchemeInvalid() {
        assertThrows(InvalidInputException.class, () -> Url.parse("wat://h"));
        assertThrows(InvalidInputException.class, () -> Url.parse("no-scheme-here"));
    }
}
