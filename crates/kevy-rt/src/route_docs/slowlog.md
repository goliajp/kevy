`SLOWLOG GET / LEN / RESET / HELP`. The sub-command + parsed
args are pre-decoded at routing time so the runtime knows
whether to short-circuit (HELP / error) or fan out across
shards (GET / LEN / RESET). See [`SlowlogSub::parse`].

```
use kevy_rt::{Argv, Route, SlowlogSub};

let argv = Argv::from(vec![b"SLOWLOG".to_vec(), b"LEN".to_vec()]);
assert_eq!(Route::Slowlog(SlowlogSub::parse(&argv)), Route::Slowlog(SlowlogSub::Len));
```
