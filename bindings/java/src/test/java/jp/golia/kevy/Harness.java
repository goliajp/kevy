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
        try (ServerSocket s = new ServerSocket(0)) {
            return s.getLocalPort();
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    /** Spawn a server; `toml` (nullable) is written as kevy.toml + --config. */
    static Server spawn(String toml, String... extra) {
        String bin = serverBin();
        if (bin == null) throw new IllegalStateException("no kevy server binary");
        try {
            Path dir = Files.createTempDirectory("kevy-java-it");
            int port = freePort();
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
            Process proc = new ProcessBuilder(cmd).redirectErrorStream(true)
                .redirectOutput(ProcessBuilder.Redirect.DISCARD).start();
            String url = "kevy://127.0.0.1:" + port;
            waitReady(url, proc);
            return new Server(proc, port, url, dir);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    private static void waitReady(String url, Process proc) {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (System.nanoTime() < deadline) {
            if (!proc.isAlive()) throw new IllegalStateException("kevy server exited early");
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
