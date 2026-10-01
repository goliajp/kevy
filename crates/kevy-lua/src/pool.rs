//! The per-dialect VM pool behind [`Bridge`]: which slot a dialect
//! lives in, how a sandbox VM is built on first use, and how the
//! bridge reports itself.

use crate::{Bridge, DISPATCH_KEY, DispatchSlot, cjson, cmsgpack, host};
use luna_core::version::LuaVersion;
use luna_core::vm::exec::Vm;
use std::rc::Rc;

pub(crate) fn dialect_slot(v: LuaVersion) -> usize {
    match v {
        LuaVersion::Lua51 => 0,
        LuaVersion::Lua52 => 1,
        LuaVersion::Lua53 => 2,
        LuaVersion::Lua54 => 3,
        LuaVersion::MacroLua => 4,
        LuaVersion::Lua55 => 5,
    }
}

impl Bridge {
    /// Number of dialect VMs currently spawned. Test-only helper —
    /// production code doesn't need to inspect the pool.
    #[cfg(test)]
    fn vm_count(&self) -> usize {
        self.vms.iter().filter(|s| s.is_some()).count()
    }

    /// Lazily build the sandbox Vm for `version`. Conservative
    /// default: base + math + string + table libraries, no JIT,
    /// no bytecode loading, 200M instruction budget (~5 s on modern
    /// hardware — Redis's default `lua-time-limit`). The `redis`
    /// host table is installed once at Vm-construction time; KEYS /
    /// ARGV are re-bound per `eval` call (see [`Bridge::eval`]).
    pub(crate) fn vm_for(&mut self, version: LuaVersion) -> &mut Vm {
        let slot = &mut self.vms[dialect_slot(version)];
        if slot.is_none() {
            let mut builder =
                Vm::sandbox(version).open_base().open_math().open_string().open_table();
            if self.instr_budget > 0 {
                builder = builder.with_instr_budget(self.instr_budget);
            }
            let mut vm = builder.build();
            host::install_redis_table(&mut vm);
            // BullMQ + Sidekiq Pro require the `cmsgpack` global.
            cmsgpack::install_cmsgpack(&mut vm);
            cjson::install_cjson(&mut vm);
            // Install the dispatch handle as a userdata global.
            // `redis.call` retrieves it via
            // `vm.userdata_borrow::<DispatchSlot>(DISPATCH_KEY)`. We
            // clone the Rc so each Vm holds an independent handle
            // pointing at the shared closure.
            let _ = vm.set_userdata(
                DISPATCH_KEY,
                DispatchSlot {
                    f: Rc::clone(&self.dispatch),
                    read_only: Rc::clone(&self.read_only),
                },
            );
            *slot = Some(vm);
        }
        slot.as_mut().expect("just-inserted Vm")
    }
}

pub(crate) fn format_lua_error(e: &luna_core::vm::error::LuaError) -> String {
    // luna-core impls `Display for LuaError` — embedders don't
    // need to case-split on the inner Value type.
    format!("{e}")
}

pub(crate) fn version_tag(v: LuaVersion) -> &'static str {
    match v {
        LuaVersion::Lua51 => "5.1",
        LuaVersion::Lua52 => "5.2",
        LuaVersion::Lua53 => "5.3",
        LuaVersion::Lua54 => "5.4",
        LuaVersion::MacroLua => "macro",
        LuaVersion::Lua55 => "5.5",
    }
}

impl core::fmt::Debug for Bridge {
    /// Reports the bridge's configuration and how much of it is live,
    /// without touching the VMs or the dispatch closure.
    ///
    /// Neither can be printed: luna-core's `Vm` has no `Debug`, and
    /// `dispatch` is an `Rc<dyn Fn>` with no identity worth showing. The
    /// VMs are reported as a count of spawned slots, which is the thing
    /// worth knowing about them from outside — whether a dialect has been
    /// used yet.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bridge")
            .field("vms_spawned", &self.vms.iter().filter(|v| v.is_some()).count())
            .field("read_only", &self.read_only.get())
            .field("instr_budget", &self.instr_budget)
            .field("allow", &self.allow)
            .field("cached_scripts", &self.script_cache.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use crate::{Bridge, FlushMode};

    #[test]
    fn eval_reuses_vm_across_calls() {
        let mut b = Bridge::with_no_dispatch();
        assert_eq!(b.eval(b"return 1", &[], &[]), b":1\r\n");
        assert_eq!(b.eval(b"return 2", &[], &[]), b":2\r\n");
        // One VM should be cached for the 5.1 default dialect.
        assert_eq!(b.vm_count(), 1);
    }

    #[test]
    fn script_flush_drops_vm_pool() {
        let mut b = Bridge::with_no_dispatch();
        let _ = b.eval(b"return 1", &[], &[]);
        assert_eq!(b.vm_count(), 1);
        b.script_flush(FlushMode::Sync);
        assert_eq!(b.vm_count(), 0);
    }
}
