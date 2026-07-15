// RESP2/RESP3 codec conformance (client-contract §4.1). Pure — no backend.
package jp.golia.kevy;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.charset.StandardCharsets;
import java.util.List;
import org.junit.jupiter.api.Test;

class ReplyParserTest {
    private static Reply parse(String wire) {
        byte[] b = wire.getBytes(StandardCharsets.UTF_8);
        RespParser.Parsed p = RespParser.parse(b, 0, b.length);
        assertTrue(p.used() > 0, "expected a complete frame");
        return p.reply();
    }

    @Test void simpleString() {
        assertEquals("OK", Bytes.str(((Reply.Simple) parse("+OK\r\n")).value()));
    }

    @Test void errorFrame() {
        assertEquals("ERR bad", Bytes.str(((Reply.Error) parse("-ERR bad\r\n")).text()));
    }

    @Test void integer() {
        assertEquals(42, ((Reply.Int) parse(":42\r\n")).value());
    }

    @Test void bulkAndNil() {
        assertEquals("foo", Bytes.str(((Reply.Bulk) parse("$3\r\nfoo\r\n")).value()));
        assertTrue(parse("$-1\r\n").isNil());
    }

    @Test void array() {
        Reply.Array a = (Reply.Array) parse("*2\r\n$1\r\na\r\n:2\r\n");
        assertEquals(2, a.items().size());
        assertEquals("a", Bytes.str(a.items().get(0).payload()));
        assertEquals(2, ((Reply.Int) a.items().get(1)).value());
    }

    @Test void needMore() {
        byte[] b = "$3\r\nfo".getBytes(StandardCharsets.UTF_8);
        assertEquals(0, RespParser.parse(b, 0, b.length).used());
    }

    @Test void resp3Map() {
        Reply.Map m = (Reply.Map) parse("%1\r\n$1\r\na\r\n:1\r\n");
        assertEquals(1, m.entries().size());
    }

    @Test void resp3Scalars() {
        assertEquals(3.14, ((Reply.Double) parse(",3.14\r\n")).value(), 1e-9);
        assertTrue(((Reply.Bool) parse("#t\r\n")).value());
        assertInstanceOf(Reply.Null.class, parse("_\r\n"));
        assertEquals("12345", Bytes.str(((Reply.BigNumber) parse("(12345\r\n")).text()));
    }

    @Test void pushFrame() {
        Reply.Push p = (Reply.Push) parse(">3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$2\r\nhi\r\n");
        assertEquals("message", Bytes.str(p.items().get(0).payload()));
    }

    @Test void verbatim() {
        Reply.Verbatim v = (Reply.Verbatim) parse("=15\r\ntxt:hello world\r\n");
        assertEquals("txt", Bytes.str(v.format()));
        assertEquals("hello world", Bytes.str(v.text()));
    }

    @Test void attributeSkipped() {
        // RESP3 attribute frame is parsed and discarded; the decorated reply surfaces.
        assertEquals("OK", Bytes.str(((Reply.Simple) parse("|1\r\n$1\r\na\r\n:1\r\n+OK\r\n")).value()));
    }

    @Test void encodeRoundTrips() {
        byte[] wire = RespParser.encodeCommand(List.of(Bytes.of("SET"), Bytes.of("k"), Bytes.of("v")));
        assertEquals("*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n", new String(wire, StandardCharsets.UTF_8));
    }
}
