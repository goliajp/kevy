// Url — connect-URL parsing and scheme routing (client-contract §1.1–1.2).
// One connect(url) entry point picks the backend from the scheme; rejections
// (TLS, AUTH, unknown scheme, empty file path) happen here, before any I/O.
package jp.golia.kevy;

import java.io.File;
import java.io.IOException;

public final class Url {
    private Url() {}

    /** A resolved connection target. */
    public sealed interface Target permits MemAnon, MemNamed, FileStore, Remote {
        /** The process-global registry key, or null for isolated/remote targets. */
        String registryKey();
    }

    /** Anonymous in-memory: a fresh, never-shared store each connect. */
    public record MemAnon() implements Target {
        public String registryKey() { return null; }
    }

    /** Named in-memory bus: shared by name across the process. */
    public record MemNamed(String name) implements Target {
        public String registryKey() { return "mem://" + name; }
    }

    /** Persistent store: shared by canonical path across the process. */
    public record FileStore(String path, String canonical) implements Target {
        public String registryKey() { return "file://" + canonical; }
    }

    /** Remote RESP server; `raw` (tcp://) skips SELECT and ignores /db. */
    public record Remote(String host, int port, Integer db, boolean raw) implements Target {
        public String registryKey() { return null; }
    }

    public static Target parse(String url) {
        int sep = url.indexOf("://");
        if (sep < 0) throw new InvalidInputException("unknown URL scheme: " + url);
        String scheme = url.substring(0, sep);
        String rest = url.substring(sep + 3);
        return switch (scheme) {
            case "mem" -> rest.isEmpty() ? new MemAnon() : new MemNamed(rest);
            case "file" -> parseFile(rest);
            case "kevy", "redis" -> parseRemote(rest, false);
            case "tcp" -> parseRemote(rest, true);
            case "rediss", "kevys" -> throw new UnsupportedException("kevy has no TLS");
            default -> throw new InvalidInputException("unknown URL scheme: " + scheme);
        };
    }

    private static Target parseFile(String rest) {
        if (rest.isEmpty()) throw new InvalidInputException("file:// requires a path");
        try {
            String canonical = new File(rest).getCanonicalPath();
            return new FileStore(rest, canonical);
        } catch (IOException e) {
            throw new InvalidInputException("bad file path: " + rest);
        }
    }

    private static Target parseRemote(String rest, boolean raw) {
        if (rest.indexOf('@') >= 0) throw new UnsupportedException("kevy has no AUTH");
        String authority = rest;
        String path = "";
        int slash = rest.indexOf('/');
        if (slash >= 0) {
            authority = rest.substring(0, slash);
            path = rest.substring(slash); // includes leading '/'
        }
        String host;
        int port = 6379;
        int colon = authority.lastIndexOf(':');
        if (colon >= 0) {
            host = authority.substring(0, colon);
            String p = authority.substring(colon + 1);
            try {
                port = Integer.parseInt(p);
            } catch (NumberFormatException e) {
                throw new InvalidInputException("bad port: " + p);
            }
        } else {
            host = authority;
        }
        if (host.isEmpty()) throw new InvalidInputException("empty host in URL");
        Integer db = null;
        if (!raw && path.length() > 1) {
            String n = path.substring(1);
            try {
                db = Integer.parseInt(n);
            } catch (NumberFormatException e) {
                throw new InvalidInputException("bad /db index: " + n);
            }
        }
        return new Remote(host, port, db, raw);
    }
}
