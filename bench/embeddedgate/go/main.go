// embeddedgate — Go track. kevy-go scalar GetScalar/SetScalar vs bbolt
// (mmap B+tree) and badger (LSM+vlog). Headline tier T-async: kevy AOF
// EverySec, bbolt NoSync=true, badger SyncWrites=false — all OS-flush, none
// fsyncs per op (auditable: each side's durability config printed).
//
// Neither Go peer exposes a bare synchronous get/set — both force a
// transaction closure — so we report BOTH cold-single-op (one txn per op,
// what a one-off scalar call costs) and amortized (one txn wrapping N).
// kevy's scalar has no txn, so kevy's number is the same in both — itself the
// finding. k/p < 1 means kevy faster.
//
// Relative standing from the dev host; definitive SLA = lx64 (perf §9).
// Run via ./run.sh (stages release libkevy_ffi.a for the cgo link).
package main

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"time"

	badger "github.com/dgraph-io/badger/v4"
	kevy "github.com/goliajp/kevy-go"
	bolt "go.etcd.io/bbolt"
)

const (
	N    = 100_000
	KEYS = 200
	RUNS = 3
)

var sizes = []int{16, 256, 4096, 65536}

func keyset() [][]byte {
	ks := make([][]byte, KEYS)
	for i := range ks {
		ks[i] = []byte(fmt.Sprintf("k:%d", i))
	}
	return ks
}

// median-of-RUNS ns/op
func timeit(fn func()) float64 {
	s := make([]float64, RUNS)
	for r := 0; r < RUNS; r++ {
		t0 := time.Now()
		fn()
		s[r] = float64(time.Since(t0).Nanoseconds()) / float64(N)
	}
	sort.Float64s(s)
	return s[1]
}

type res struct{ getCold, getAmort, setCold, setAmort float64 }

func benchKevy(db *kevy.DB, ks [][]byte, v []byte) res {
	for _, k := range ks {
		_ = db.SetScalar(k, v, 0)
	}
	get := timeit(func() {
		for i := 0; i < N; i++ {
			_, _, _ = db.GetScalar(ks[i%KEYS])
		}
	})
	setCold := timeit(func() {
		for i := 0; i < N; i++ {
			_ = db.SetScalar(ks[i%KEYS], v, 0)
		}
	})
	// amortized: SetMany — one cgo crossing for all N sets (the batch path)
	mk := make([][]byte, N)
	mv := make([][]byte, N)
	for i := 0; i < N; i++ {
		mk[i] = ks[i%KEYS]
		mv[i] = v
	}
	setAmort := timeit(func() { _ = db.SetMany(mk, mv) })
	return res{get, get, setCold, setAmort} // GET: no read txn to amortize (cold==amort)
}

var bucket = []byte("kv")

func benchBolt(db *bolt.DB, ks [][]byte, v []byte) res {
	_ = db.Update(func(tx *bolt.Tx) error {
		b, _ := tx.CreateBucketIfNotExists(bucket)
		for _, k := range ks {
			_ = b.Put(k, v)
		}
		return nil
	})
	getCold := timeit(func() {
		for i := 0; i < N; i++ {
			_ = db.View(func(tx *bolt.Tx) error {
				_ = tx.Bucket(bucket).Get(ks[i%KEYS])
				return nil
			})
		}
	})
	// amortized: one View txn reused for all N gets
	getAmort := timeit(func() {
		_ = db.View(func(tx *bolt.Tx) error {
			b := tx.Bucket(bucket)
			for i := 0; i < N; i++ {
				_ = b.Get(ks[i%KEYS])
			}
			return nil
		})
	})
	// cold single-op: one Update (commit) per Put
	setCold := timeit(func() {
		for i := 0; i < N; i++ {
			_ = db.Update(func(tx *bolt.Tx) error {
				return tx.Bucket(bucket).Put(ks[i%KEYS], v)
			})
		}
	})
	// amortized: one Update wrapping all N Puts
	setAmort := timeit(func() {
		_ = db.Update(func(tx *bolt.Tx) error {
			b := tx.Bucket(bucket)
			for i := 0; i < N; i++ {
				_ = b.Put(ks[i%KEYS], v)
			}
			return nil
		})
	})
	return res{getCold, getAmort, setCold, setAmort}
}

func benchBadger(db *badger.DB, ks [][]byte, v []byte) res {
	_ = db.Update(func(txn *badger.Txn) error {
		for _, k := range ks {
			_ = txn.Set(k, v)
		}
		return nil
	})
	getCold := timeit(func() {
		for i := 0; i < N; i++ {
			_ = db.View(func(txn *badger.Txn) error {
				item, err := txn.Get(ks[i%KEYS])
				if err == nil {
					_, _ = item.ValueCopy(nil)
				}
				return nil
			})
		}
	})
	// amortized: one View txn reused for all N gets
	getAmort := timeit(func() {
		_ = db.View(func(txn *badger.Txn) error {
			for i := 0; i < N; i++ {
				item, err := txn.Get(ks[i%KEYS])
				if err == nil {
					_, _ = item.ValueCopy(nil)
				}
			}
			return nil
		})
	})
	setCold := timeit(func() {
		for i := 0; i < N; i++ {
			_ = db.Update(func(txn *badger.Txn) error {
				return txn.Set(ks[i%KEYS], v)
			})
		}
	})
	setAmort := timeit(func() {
		wb := db.NewWriteBatch()
		for i := 0; i < N; i++ {
			_ = wb.Set(ks[i%KEYS], v)
		}
		_ = wb.Flush()
	})
	return res{getCold, getAmort, setCold, setAmort}
}

func verdict(k, p float64) string {
	r := k / p
	switch {
	case r < 0.97:
		return fmt.Sprintf("kevy %.2f×", p/k)
	case r > 1.03:
		return fmt.Sprintf("peer %.2f×", k/p)
	default:
		return "tie"
	}
}

func table(name, field string, kevy map[int]res, peer map[int]res, get func(res) float64) {
	fmt.Printf("\n### %s — %s\n", name, field)
	fmt.Println("|   size | kevy ns | peer ns | k/p | verdict |")
	fmt.Println("|-------:|--------:|--------:|----:|---------|")
	for _, s := range sizes {
		k, p := get(kevy[s]), get(peer[s])
		fmt.Printf("| %6d | %7.0f | %7.0f | %4.2f | %s |\n", s, k, p, k/p, verdict(k, p))
	}
}

func main() {
	ks := keyset()
	dir, _ := os.MkdirTemp("", "embgate-go-")
	defer os.RemoveAll(dir)

	fmt.Println("# embeddedgate — Go — kevy-go scalar vs bbolt / badger")
	fmt.Printf("N=%d ops/measurement, %d warm keys, median-of-%d, sizes 16/256/4096/65536 B\n", N, KEYS, RUNS)
	fmt.Println("kevy: cold==amortized (no per-op txn). bbolt/badger: cold=one txn/op, amort=one txn/N (badger WriteBatch).")
	fmt.Println("\n## T-async — kevy AOF EverySec / bbolt NoSync=true / badger SyncWrites=false (OS-flush, no per-op fsync)")

	kevyR := map[int]res{}
	boltR := map[int]res{}
	badgerR := map[int]res{}
	for _, s := range sizes {
		v := make([]byte, s)
		for i := range v {
			v[i] = 0x61
		}
		// kevy
		kdb, err := kevy.Open(filepath.Join(dir, fmt.Sprintf("kevy-%d", s)))
		if err != nil {
			panic(err)
		}
		kevyR[s] = benchKevy(kdb, ks, v)
		kdb.Close()
		// bbolt (NoSync = T-async)
		bdb, err := bolt.Open(filepath.Join(dir, fmt.Sprintf("bolt-%d.db", s)), 0600, nil)
		if err != nil {
			panic(err)
		}
		bdb.NoSync = true
		boltR[s] = benchBolt(bdb, ks, v)
		_ = bdb.Close()
		// badger (SyncWrites=false default = T-async)
		opt := badger.DefaultOptions(filepath.Join(dir, fmt.Sprintf("badger-%d", s))).
			WithLogger(nil).WithSyncWrites(false)
		gdb, err := badger.Open(opt)
		if err != nil {
			panic(err)
		}
		badgerR[s] = benchBadger(gdb, ks, v)
		_ = gdb.Close()
	}

	fmt.Println("\n## kevy vs bbolt")
	table("bbolt", "GET cold-1op", kevyR, boltR, func(r res) float64 { return r.getCold })
	table("bbolt", "GET amortized", kevyR, boltR, func(r res) float64 { return r.getAmort })
	table("bbolt", "SET cold-1op", kevyR, boltR, func(r res) float64 { return r.setCold })
	table("bbolt", "SET amortized", kevyR, boltR, func(r res) float64 { return r.setAmort })

	fmt.Println("\n## kevy vs badger")
	table("badger", "GET cold-1op", kevyR, badgerR, func(r res) float64 { return r.getCold })
	table("badger", "GET amortized", kevyR, badgerR, func(r res) float64 { return r.getAmort })
	table("badger", "SET cold-1op", kevyR, badgerR, func(r res) float64 { return r.setCold })
	table("badger", "SET amortized", kevyR, badgerR, func(r res) float64 { return r.setAmort })

	fmt.Println("\n(relative standing — dev host; definitive SLA = lx64 per perf §9)")
}
