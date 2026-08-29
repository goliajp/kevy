// Harness — conformance-test support. Spawns a real kevy server for remote
// tests (mirroring the Go/Python harnesses) and parametrizes the backend
// families over BOTH backends: an isolated embedded mem:// store and a fresh
// remote connection to a shared server (flushed per test). Remote is skipped
// when no server binary is available (KEVY_SERVER_BIN), so embedded still runs.
package jp.golia.kevy;

import java.io.File;
import java.io.IOException;
import java.net.ServerSocket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Supplier;
import java.util.stream.Stream;

import org.junit.jupiter.api.Named;
import org.junit.jupiter.params.provider.Arguments;

final class Harness {
    private Harness() {}

    private static final AtomicInteger SEQ = new AtomicInteger();
    private static volatile Server shared;

    /** A spawned server process bound to a free loopback port. */
    static final class Server implements AutoCloseable {
        final Process proc;
        final int port;
        final String url;
        final Path dir;

        Server(Process proc, int port, String url, Path dir) {
            this.proc = proc;
            this.port = port;
            this.url = url;
            this.dir = dir;
        }

        @Override
        public void close() {
            proc.destroy();
        }
    }

    static String serverBin() {
        String b = System.getenv("KEVY_SERVER_BIN");
        return (b != null && new File(b).canExecute()) ? b : null;
    }

    static boolean remoteAvailable() {
        return serverBin() != null;
    }

    static int freePort() {
        return freePortRun(1);
    }

    /**
     * A run of {@code n} CONSECUTIVE ports, every one of them free at the
     * moment this returns.
     *
     * A `--cluster` server binds its main port P and then P+1..P+threads.
     * This harness asked the kernel for one ephemeral port and handed it
     * over as P, and nothing ever looked at P+1. When any of those was
     * taken the server failed to bind and died, and the only thing the
     * test could say was "kevy server exited early".
     *
     * The window between closing these probes and the server binding is
     * still there — a port cannot be held for another process — but the
     * failure this closes was not a race. It was never checking.
     */
    static int freePortRun(int n) {
        for (int attempt = 0; attempt < 200; attempt++) {
            int base;
            try (ServerSocket probe = new ServerSocket(0)) {
                base = probe.getLocalPort();
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
            if (base + n > 65_535) continue;
            List<ServerSocket> held = new ArrayList<>();
            boolean all = true;
            for (int i = 0; i < n && all; i++) {
                try {
                    held.add(new ServerSocket(base + i));
                } catch (IOException taken) {
                    all = false;
                }
            }
            for (ServerSocket s : held) {
                try {
                    s.close();
                } catch (IOException ignored) {
                    // closing a probe cannot fail in a way that matters
                }
            }
            if (all) return base;
        }
        throw new IllegalStateException(
            "no run of " + n + " consecutive free ports after 200 attempts");
    }

    /** How many consecutive ports a server started with these flags needs. */
    private static int portsNeeded(String... extra) {
        boolean cluster = false;
        int threads = 1;
        for (int i = 0; i < extra.length; i++) {
            if ("--cluster".equals(extra[i])) cluster = true;
            if ("--threads".equals(extra[i]) && i + 1 < extra.length) {
                try {
                    threads = Integer.parseInt(extra[i + 1]);
                } catch (NumberFormatException ignored) {
                    // leave the default; the server will refuse it itself
                }
            }
        }
        // Cluster mode: the main port, then one per shard.
        return cluster ? 1 + threads : 1;
    }

    /** Spawn a server; `toml` (nullable) is written as kevy.toml + --config. */
    static Server spawn(String toml, String... extra) {
        String bin = serverBin();
        if (bin == null) throw new IllegalStateException("no kevy server binary");
        try {
            Path dir = Files.createTempDirectory("kevy-java-it");
            int port = freePortRun(portsNeeded(extra));
            List<String> cmd = new ArrayList<>();
            cmd.add(bin);
            cmd.add("--bind");
            cmd.add("127.0.0.1");
            cmd.add("--port");
            cmd.add(Integer.toString(port));
            cmd.add("--dir");
            cmd.add(dir.toString());
            if (toml != null) {
                Path cfg = dir.resolve("kevy.toml");
                Files.writeString(cfg, toml);
                cmd.add("--config");
                cmd.add(cfg.toString());
            }
            for (String e : extra) cmd.add(e);
            // The server's own output used to go to DISCARD, so "exited
            // early" was all a failing test could ever say. It goes to a
            // file beside the data dir now, and the exception carries its
            // tail — the reason is usually in the first line.
            Path log = dir.resolve("server.log");
            Process proc = new ProcessBuilder(cmd).redirectErrorStream(true)
                .redirectOutput(log.toFile()).start();
            String url = "kevy://127.0.0.1:" + port;
            waitReady(url, proc, log, port);
            return new Server(proc, port, url, dir);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    /** The last few lines a dead server wrote, or why they cannot be read. */
    private static String tail(Path log) {
        try {
            List<String> lines = Files.readAllLines(log);
            int from = Math.max(0, lines.size() - 8);
            if (lines.isEmpty()) return "  (the server wrote nothing before exiting)";
            StringBuilder b = new StringBuilder();
            for (String l : lines.subList(from, lines.size())) b.append("  ").append(l).append('\n');
            return b.toString();
        } catch (IOException e) {
            return "  (its log could not be read: " + e + ")";
        }
    }

    private static void waitReady(String url, Process proc, Path log, int port) {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (System.nanoTime() < deadline) {
            if (!proc.isAlive()) {
                throw new IllegalStateException(
                    "kevy server on port " + port + " exited early (status "
                        + proc.exitValue() + "). Its own words:\n" + tail(log));
            }
            try (KevyClient c = Kevy.connect(url)) {
                c.ping();
                return;
            } catch (RuntimeException retry) {
                sleep(100);
            }
        }
        throw new IllegalStateException("kevy server not ready within 10s: " + url);
    }

    /** A lazily-started, shared default server for the common remote tests. */
    static Server shared() {
        Server s = shared;
        if (s == null) {
            synchronized (Harness.class) {
                if (shared == null) {
                    shared = spawn(null);
                    Runtime.getRuntime().addShutdownHook(new Thread(shared::close));
                }
                s = shared;
            }
        }
        return s;
    }

    static KevyClient newEmbedded() {
        return Kevy.connect("mem://t-" + SEQ.incrementAndGet());
    }

    static KevyClient newRemote() {
        KevyClient c = Kevy.connect(shared().url);
        c.flushall();
        return c;
    }

    /** @MethodSource for family tests: embedded always, remote when available. */
    static Stream<Arguments> bothBackends() {
        List<Arguments> l = new ArrayList<>();
        l.add(Arguments.of(Named.of("embedded", (Supplier<KevyClient>) Harness::newEmbedded)));
        if (remoteAvailable()) {
            l.add(Arguments.of(Named.of("remote", (Supplier<KevyClient>) Harness::newRemote)));
        }
        return l.stream();
    }

    static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }
}
