// PipelineBuf — non-atomic client-side batching, remote-only (client-contract
// §3.13). Queue N commands, send as one write, read N replies in order. NOT
// atomic (the server may interleave other clients). Per-command -ERR come back
// INLINE as Reply.Error entries (they do NOT abort the batch) — unlike the
// typed single-command wraps. An empty batch returns [] without touching the
// wire; an empty-argv command poisons the batch → InvalidInput at send.
package jp.golia.kevy;

import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;

public final class PipelineBuf {
    private final List<List<byte[]>> cmds = new ArrayList<>();
    private boolean poisoned;

    PipelineBuf() {}

    /** Append a command (raw argv). Empty argv poisons the batch. */
    public PipelineBuf cmd(byte[]... parts) {
        if (parts.length == 0) {
            poisoned = true;
        } else {
            cmds.add(new ArrayList<>(List.of(parts)));
        }
        return this;
    }

    public PipelineBuf cmd(String... parts) {
        byte[][] b = new byte[parts.length][];
        for (int i = 0; i < parts.length; i++) b[i] = Bytes.of(parts[i]);
        return cmd(b);
    }

    public int len() {
        return cmds.size();
    }

    public boolean isEmpty() {
        return cmds.isEmpty();
    }

    static List<Reply> run(KevyClient client, Consumer<PipelineBuf> build) {
        if (client.isEmbedded()) throw new UnsupportedException("pipeline is remote-only");
        PipelineBuf buf = new PipelineBuf();
        build.accept(buf);
        if (buf.poisoned) throw new InvalidInputException("pipeline has an empty-argv command");
        if (buf.cmds.isEmpty()) return new ArrayList<>();
        ByteArrayOutputStream wire = new ByteArrayOutputStream();
        try {
            for (List<byte[]> argv : buf.cmds) RespParser.encodeCommand(wire, argv);
        } catch (java.io.IOException e) {
            throw new java.io.UncheckedIOException(e); // ByteArrayOutputStream never throws
        }
        return client.backend().remoteConn().pipelineRaw(wire.toByteArray(), buf.cmds.size());
    }
}
