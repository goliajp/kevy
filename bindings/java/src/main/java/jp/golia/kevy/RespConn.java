// RespConn — the blocking RESP2/3 TCP transport (client-contract §1.1 remote).
// A hand-rolled socket client: encode a command's argv, write it, and read
// replies back with the shared incremental parser feeding a growable buffer
// (the exact "parse → need-more → read more" loop the other ports use). Used
// by the command connection, the Subscriber, the pipeline, and each cluster
// shard.
package jp.golia.kevy;

import java.io.BufferedOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

final class RespConn implements AutoCloseable {
    private final Socket sock;
    private final InputStream in;
    private final OutputStream out;
    private byte[] rbuf = new byte[8192];
    private int rstart = 0;
    private int rend = 0;

    private RespConn(Socket sock) throws IOException {
        this.sock = sock;
        this.in = sock.getInputStream();
        this.out = new BufferedOutputStream(sock.getOutputStream());
    }

    static RespConn dial(String host, int port) {
        try {
            Socket s = new Socket();
            s.setTcpNoDelay(true);
            s.connect(new InetSocketAddress(host, port), 10_000);
            return new RespConn(s);
        } catch (IOException e) {
            throw new KevyIoException("connect to " + host + ":" + port + " failed", e);
        }
    }

    /** Send a command and read exactly one reply. */
    Reply request(List<byte[]> argv) {
        writeCommand(argv);
        flush();
        return readOne();
    }

    /** Write a command without reading its reply (Subscriber subscribe/etc.). */
    void writeOnly(List<byte[]> argv) {
        writeCommand(argv);
        flush();
    }

    /** Send a pre-encoded batch of `count` commands and read `count` replies. */
    List<Reply> pipelineRaw(byte[] wire, int count) {
        try {
            out.write(wire);
        } catch (IOException e) {
            throw mapIo(e);
        }
        flush();
        List<Reply> replies = new ArrayList<>(count);
        for (int i = 0; i < count; i++) replies.add(readOne());
        return replies;
    }

    /** Issue SELECT N on connect; a -ERR reply fails the connect. */
    void selectDb(int db) {
        Reply r = request(List.of(Bytes.of("SELECT"), Bytes.ofLong(db)));
        if (r.isError()) throw Errors.fromReplyText(r.payload());
    }

    /** Bound blocking reads; null clears the deadline. */
    void setReadTimeout(java.time.Duration dur) {
        try {
            sock.setSoTimeout(dur == null ? 0 : (int) Math.max(1, dur.toMillis()));
        } catch (IOException e) {
            throw mapIo(e);
        }
    }

    private void writeCommand(List<byte[]> argv) {
        try {
            RespParser.encodeCommand(out, argv); // straight into the buffered stream, no intermediate byte[]
        } catch (IOException e) {
            throw mapIo(e);
        }
    }

    private void flush() {
        try {
            out.flush();
        } catch (IOException e) {
            throw mapIo(e);
        }
    }

    /** Read one full reply, refilling the buffer from the socket as needed. */
    Reply readOne() {
        while (true) {
            RespParser.Parsed p = RespParser.parse(rbuf, rstart, rend);
            if (p.used() > 0) {
                rstart += p.used();
                return p.reply();
            }
            fill();
        }
    }

    private void fill() {
        if (rstart > 0) {
            System.arraycopy(rbuf, rstart, rbuf, 0, rend - rstart);
            rend -= rstart;
            rstart = 0;
        }
        if (rend == rbuf.length) {
            rbuf = Arrays.copyOf(rbuf, rbuf.length * 2);
        }
        int n;
        try {
            n = in.read(rbuf, rend, rbuf.length - rend);
        } catch (SocketTimeoutException e) {
            throw new TimedOutException("read timed out");
        } catch (IOException e) {
            throw mapIo(e);
        }
        if (n < 0) {
            closeQuietly();
            throw new ClosedException("server closed connection mid-read");
        }
        rend += n;
    }

    private KevyException mapIo(IOException e) {
        return new KevyIoException(e.getMessage() == null ? e.toString() : e.getMessage(), e);
    }

    private void closeQuietly() {
        try {
            sock.close();
        } catch (IOException ignored) {
            // best effort
        }
    }

    @Override
    public void close() {
        closeQuietly();
    }
}
