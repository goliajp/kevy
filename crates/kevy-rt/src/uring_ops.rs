//! io_uring reactor constants shared across the uring modules: the
//! `user_data` op-tag layout (`OP_*` / `CONN_MASK`), the writev iovec
//! cap, and the errno the buf-ring path special-cases. Split out of
//! [`crate::uring_reactor`] (500-LOC house rule); that module
//! re-exports everything, so `crate::uring_reactor::OP_RECV`-style
//! paths keep working.

/// `-ENOBUFS`: the buf ring was momentarily empty; just re-arm (don't close).
pub(crate) const ENOBUFS: i32 = 105;

/// **A.4 (v1.25)**: maximum iovec entries packed into one `writev` SQE.
/// Linux caps `writev(2)` (and `IORING_OP_WRITEV`) at `IOV_MAX = 1024`
/// vectors; over the cap the kernel returns `-EINVAL`. The chunked-
/// writev state machine on `UringConn` (`arcs_in_flight` +
/// `write_byte_cap` + `write_inflight_bytes`) submits one chunk per
/// arm_conns iter and drops the processed prefix in the CQE handler,
/// so a 50-sub × 1024-publish pub/sub burst lands in ~2 SQEs per conn
/// (TCP-ordered, never violating IOV_MAX) instead of forcing the
/// in-shard memcpy fallback after the prior 256-arc cap.
pub(crate) const MAX_IOVECS_PER_WRITEV: usize = 1024;


// `user_data` layout: top 4 bits = op, low 60 bits = conn id.
// v1.29 B2-alt widened OP_SHIFT from 61 to 60 to add two new op tags
// (`OP_BIG_CANCEL` + `OP_BIG_READ`) for the bigval-SET kernel-direct
// recv state machine. The 60-bit conn id space (~1.15 × 10^18) stays
// orders of magnitude beyond any realistic next_conn_id growth rate.
pub(crate) const OP_SHIFT: u32 = 60;
pub(crate) const OP_RECV: u64 = 1 << OP_SHIFT;
pub(crate) const OP_WRITE: u64 = 2 << OP_SHIFT;
pub(crate) const OP_ACCEPT: u64 = 3 << OP_SHIFT;
/// The shard's waker pipe became readable (a peer woke a parked shard).
pub(crate) const OP_WAKER: u64 = 4 << OP_SHIFT;
/// The bounded-park timeout fired (see [`ParkState`]).
pub(crate) const OP_TIMEOUT: u64 = 5 << OP_SHIFT;
/// Accept on the per-shard cluster listener (conns marked for `-MOVED`).
pub(crate) const OP_ACCEPT_CL: u64 = 6 << OP_SHIFT;
/// v1.25 UDS: accept on the (shard-0-only) Unix-domain listener.
pub(crate) const OP_ACCEPT_UN: u64 = 7 << OP_SHIFT;
/// **v1.29 B2-alt**: the cancel SQE issued to abort a multishot recv
/// before switching the conn to single-shot `prep_read` for big-arg
/// ingest. The CQE for THIS SQE reports `res = 0` on successful match
/// or `-ENOENT` if the target multishot already terminated. The target
/// multishot's terminal `-ECANCELED` CQE arrives under the existing
/// `OP_RECV` tag (handled in `uring_on_recv`).
pub(crate) const OP_BIG_CANCEL: u64 = 8 << OP_SHIFT;
/// **v1.29 B2-alt**: the single-shot `prep_read` SQE that draws body
/// bytes directly from the kernel into the destination `Vec<u8>` (no
/// userspace memcpy through the provided-buffer slab). Re-submitted
/// until the entire body is received; the multishot is re-armed on
/// completion to accept any pipelined commands after the big body.
pub(crate) const OP_BIG_READ: u64 = 9 << OP_SHIFT;
pub(crate) const CONN_MASK: u64 = (1 << OP_SHIFT) - 1;
