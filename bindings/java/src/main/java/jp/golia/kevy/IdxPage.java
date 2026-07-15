// IdxPage — one page of an IDX.QUERY RANGE/EQ scan (client-contract §4.4).
// `cursor` is null when the scan is complete (wire cursor "0"); otherwise pass
// it back to idx_query_range/_eq to resume. Rows are in (value, key) order.
package jp.golia.kevy;

import java.util.List;

public record IdxPage(byte[] cursor, List<IdxRow> rows) {
    /** True when the scan is complete (no further page). */
    public boolean done() {
        return cursor == null;
    }
}
