//! Bare-metal Cortex-M4 firmware that runs `kevy-store` for real.
//!
//! The IoT docs claimed the `no_std` core was "proven on a bare-metal
//! Cortex-M target", but the proof was a `cargo check` — the code had
//! never executed on an MCU. This firmware closes that: it boots on a
//! Cortex-M4 (QEMU `mps2-an386`), stands up a real `kevy_store::Store`,
//! and exercises the KV + TTL surface, reporting over semihosting.
//!
//! Everything the runtime needs is hand-written, because kevy takes no
//! dependencies — no `cortex-m-rt`, no `embedded-alloc`:
//!
//!   * the reset vector and the initial stack pointer,
//!   * a bump allocator (an MCU has no OS heap),
//!   * a panic handler,
//!   * semihosting I/O to get output off the chip.
//!
//! Run:  cd bench/mcu-probe && cargo run --release

#![no_std]
#![no_main]

extern crate alloc;

use alloc::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;
use core::time::Duration;

// ---------------------------------------------------------------- heap

/// The whole heap the store gets. A real device would size this to the
/// board; 64 KiB is a mid-range Cortex-M4's SRAM budget.
const HEAP_SIZE: usize = 64 * 1024;
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
static mut HEAP_USED: usize = 0;

/// Bump allocator: allocation is a pointer add, free is a no-op. That is
/// the honest shape for a probe — it proves the store runs against a
/// caller-supplied heap without dragging in an allocator crate.
struct Bump;

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = &raw mut HEAP as usize;
        let used = unsafe { HEAP_USED };
        let start = (base + used).next_multiple_of(layout.align());
        let end = start + layout.size() - base;
        if end > HEAP_SIZE {
            return core::ptr::null_mut();
        }
        unsafe { HEAP_USED = end };
        start as *mut u8
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOC: Bump = Bump;

fn heap_used() -> usize {
    unsafe { HEAP_USED }
}

// -------------------------------------------------------- semihosting

/// ARM semihosting: `bkpt #0xAB` with the operation in r0. This is how a
/// bare-metal target talks to the host debugger/emulator.
fn sys_call(op: u32, arg: u32) -> u32 {
    let ret: u32;
    unsafe {
        core::arch::asm!(
            "bkpt #0xAB",
            inlateout("r0") op => ret,
            in("r1") arg,
            options(nostack, preserves_flags),
        );
    }
    ret
}

/// SYS_WRITEC — one character to the host console.
fn putc(c: u8) {
    sys_call(0x03, &c as *const u8 as u32);
}

fn print(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

fn print_num(mut n: u64) {
    if n == 0 {
        putc(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        putc(buf[i]);
    }
}

/// SYS_EXIT — hand the emulator an exit code so `cargo run` terminates.
fn exit(code: u32) -> ! {
    // ADP_Stopped_ApplicationExit, with the status in a two-word block.
    let block = [0x2002u32, code];
    sys_call(0x18, block.as_ptr() as u32);
    loop {}
}

// ---------------------------------------------------------------- boot

unsafe extern "C" {
    static mut __sbss: u8;
    static mut __ebss: u8;
}

/// The reset vector. No runtime has run yet: zero `.bss` by hand, then go.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reset() -> ! {
    unsafe {
        let start = &raw mut __sbss;
        let end = &raw mut __ebss;
        let n = end as usize - start as usize;
        core::ptr::write_bytes(start, 0, n);
    }
    main()
}

/// The reset handler slot of the vector table. Slot 0 — the initial
/// stack pointer — is a raw address, not a function, so the linker
/// script emits it directly (see `LONG(0x20400000)` in link.x) rather
/// than us trying to spell an address as an `fn` pointer here.
#[unsafe(link_section = ".vector_table")]
#[unsafe(no_mangle)]
pub static RESET_VECTOR: unsafe extern "C" fn() -> ! = reset;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    print("PANIC: ");
    if let Some(m) = info.message().as_str() {
        print(m);
    }
    print("\n");
    exit(1)
}

// ---------------------------------------------------------------- main

fn main() -> ! {
    print("== kevy-store on bare-metal Cortex-M4 ==\n");

    let heap_at_boot = heap_used();

    // A Cortex-M has no OS clock. The firmware owns time and feeds it in;
    // this is exactly what the `external-clock` feature exists for.
    kevy_store::set_clock_ns(0);

    let mut store = kevy_store::Store::new();
    let heap_after_open = heap_used();

    // The sensor-cache shape a real MCU deployment runs.
    for i in 0..64u32 {
        let key = [b's', b'e', b'n', b':', b'0' + (i / 10) as u8, b'0' + (i % 10) as u8];
        store.set(&key, b"22.5".to_vec(), None, false, false);
    }
    let heap_after_writes = heap_used();

    let hit = matches!(store.get(b"sen:07"), Ok(Some(_)));
    print("keys        = ");
    print_num(store.dbsize() as u64);
    print("\nget sen:07  = ");
    print(if hit { "hit" } else { "MISS" });

    // TTL: the firmware advances its own clock, then reaps. No threads,
    // no timers — the device's own loop drives expiry.
    store.set(b"tmp", b"gone".to_vec(), Some(Duration::from_millis(50)), false, false);
    let before = store.dbsize();
    kevy_store::set_clock_ns(100_000_000); // 100 ms later
    store.tick_expire(64, 8);
    let after = store.dbsize();

    print("\nttl         = ");
    print_num(before as u64);
    print(" -> ");
    print_num(after as u64);
    print(" after the clock advanced\n");

    print("heap: open  = ");
    print_num((heap_after_open - heap_at_boot) as u64);
    print(" B\nheap: 64 kv = ");
    print_num((heap_after_writes - heap_after_open) as u64);
    print(" B\nheap: total = ");
    print_num(heap_used() as u64);
    print(" B of ");
    print_num(HEAP_SIZE as u64);
    print("\n");

    let ok = hit && store.dbsize() == 64 && after == before - 1;
    if ok {
        print("\n== kevy-store RUNS on a bare-metal MCU ==\n");
        exit(0)
    } else {
        print("\n== FAILED ==\n");
        exit(1)
    }
}
