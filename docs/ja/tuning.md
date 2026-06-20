# kevy を極限スループットまで詰める

kevy の op あたりオーバーヘッドを目に見えて動かすツマミを並べたページ
です。各ツマミに実測値(lx64、Intel Xeon 6、Linux 6.12 / io_uring;
方法論は [`bench/REPORT.md`](../../bench/REPORT.md))と明確なコストを
書いています。本当に必要なものだけ適用してください。

## 早見表

| ツマミ                            | いつ使う                          | 効果    |
|-----------------------------------|-----------------------------------|---------|
| server を CPU セットにピン留め    | 専用ホスト、bench 同居             | 5–15%   |
| AOF オフ (`--no-aof`)             | レプリカ / 揮発キャッシュ          | 5–10%   |
| `KEVY_IO_URING=1`                 | Linux 5.13+                        | 10–30%  |
| カーネル `mitigations=off`        | 信頼できる単一テナント機           | 12–15%  |
| `io_uring` SQPOLL (予定)          | Linux 5.13+ かつ 1 コア余裕あり    | 1.5–2×  |

カーネル床を動かせるのは `mitigations=off` と SQPOLL の二つだけ。
それ以外はすべてユーザー空間サイクルを削るだけです。

## CPU ピン留め

io_uring の reactor は固定 CPU セットに留めると安定します —— NIC IRQ
→ softirq → ユーザースレッドが同じ L1/L2 にとどまるためです:

```sh
taskset -c 0-9 kevy --port 6004
```

同じマシンで bench を走らせる場合、**server と client は互いに重なら
ない範囲にピン留め** —— server を `0-9`、client を `10-15`(構成に応じ
て)。コアを共有するとスケジューラの取り合いが io_uring の効果を相殺
します。詳細は `feedback-kevy-bench-isolation`。

## `KEVY_IO_URING=1`

reactor を epoll から io_uring に切り替えます。Linux 5.13+ 必須、古い
カーネルでは静かに epoll にフォールバックします。lx64 で -c1 +10–30%、
SQPOLL (D5) の前提でもあります。

```sh
KEVY_IO_URING=1 kevy --port 6004
```

## レプリカ / キャッシュ用途では AOF オフ

既定は `--aof`(永続化)。読み取り専用レプリカや純キャッシュ用途では
書き込みごとにディスク I/O が無駄になります:

```sh
kevy --port 6004 --no-aof
```

スループットへの影響は書き込み比率次第。**テール遅延の下がり方は
中央値より顕著**。

## カーネル `mitigations=off`(Spectre / BHB)

> **動かす前に全文を読むこと。これはセキュリティのトレードオフであり、
> タダ飯ではありません。**

Linux 6.x 以降、Spectre BHB 緩和が既定で有効です。すべての syscall が
`clear_bhb_loop`(分岐履歴バッファを掃いてユーザー / カーネル境界
越しの投機実行サイドチャネル漏洩を防ぐ小さなカーネル内ループ)を
通過します。

lx64 参照機(Intel Xeon 6、Linux 6.12)では、`clear_bhb_loop` は
kevy server の `-c1` ワークロード中で**最大の CPU 消費者** ——
**13.3%**、kevy のどのユーザー空間シンボルよりも多く食べます。`-c50`
では syscall が op に対して薄められるため 約 5% に落ちます。

### 失うもの

`mitigations=off` でブートすると、**ハードウェア脆弱性緩和を全部** 切
ります:Spectre v1/v2/BHB、Meltdown、MDS、TAA、L1TF、retbleed など
全部。**許容できる状況** は以下のみ:
- 単一テナント機(カーネルを自分で握り、信頼できないコードが動かない)
- ネットワーク L3 で隔離(または信頼できるゲートウェイの背後)
- ベンチ / テスト機

**やってはいけない場所**:マルチテナントホスト、共有 CI ランナー、
信頼できないユーザーコード(ワイヤ越しの Lua eval、第三者プラグイン
読み込みなど)を扱う機械。

### 適用方法

ブートローダのカーネル cmdline(例: `/etc/default/grub` の
`GRUB_CMDLINE_LINUX_DEFAULT`)に `mitigations=off` を足して再生成:

```sh
# Debian / Ubuntu
sudo update-grub
sudo reboot
```

再起動後に確認:

```sh
cat /proc/cmdline | grep mitigations
# ... mitigations=off ...

cat /sys/devices/system/cpu/vulnerabilities/* | head
# ... "Vulnerable" または "Mitigation: ..." が無効化されているはず
```

### 実測効果

lx64 参照機で `mitigations=off` 適用後の予測スループット:

| ワークロード | Rust client -c1 | C `redis-benchmark` -c1 |
|--------------|-----------------|--------------------------|
| 前           | ~65 k ops/s     | ~67 k ops/s              |
| 後(予測)   | ~75 k ops/s     | ~78 k ops/s              |

(数字はカーネル / CPU 依存。AMD Zen 3+ と Intel Xeon BHB と
ARM N1/N2 ではコストが異なります。**自分のハードウェアで計測のこと**。)

## `io_uring` SQPOLL(予定、未出荷)

カーネルが専用スレッドで io_uring 投入キューを polling —— op 毎の
`io_uring_enter` syscall を消します。アイドル時でも 1 コア 100%
食うため opt-in feature flag (`KEVY_SQPOLL=1`) として出します。
予測ゲイン: -c1 で **1.5–2×**、-c50 は横ばい(すでにバッチ済み)。

進捗: `bench/PERF-ATTACK-LOG-2026-06-20.md` の D5 を参照。

## もう効かないこと

- `taskset` で単一コアに絞る: io_uring が並列性を失い、shared-nothing
  シャード配置のほうが速い
- THP を無効化: kevy のアロケータパターンに目に見える効果なし
- `numactl --interleave`: 多 socket でしか意味なし。lx64 は単一 socket
- slowlog を無効化: 既定でオフ(`slower-than-micros = -1`)

## 関連

- [`bench/PERF-PROFILE-2026-06-20.md`](../../bench/PERF-PROFILE-2026-06-20.md) —— このツマミ一覧を導いたフレームグラフ診断
- [`bench/PERF-ATTACK-LOG-2026-06-20.md`](../../bench/PERF-ATTACK-LOG-2026-06-20.md) —— ツマミ毎の実測ログ
- [`bench/REPORT.md`](../../bench/REPORT.md) —— ベンチ方法論
