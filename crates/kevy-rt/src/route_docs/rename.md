`RENAME source destination` / `RENAMENX source destination`. The
runtime handles the two-shard decision: same-shard renames go
through one atomic [`crate::Store::rename`] on the owning shard; cross-
shard renames use the Take→Put orchestrator (lands in v2-3b;
v2-3a emits `-CROSSSHARD ...` for that case).

```
use kevy_rt::Route;

let route = |verb: &str| Route::Rename { nx: verb == "RENAMENX" };
assert_eq!(route("RENAME"), Route::Rename { nx: false });
```
