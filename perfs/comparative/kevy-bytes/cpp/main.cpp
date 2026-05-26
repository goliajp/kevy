// Cross-language counterpart of kevy-bytes' bench: measures std::string
// (libc++ / libstdc++ — 22 / 15 byte SSO respectively) on the same five
// workloads (clone_inline_12B / clone_heap_64B / eq_inline_12B /
// eq_heap_64B / from_str_*). Emits one JSON line per measurement.
//
// Build: see Makefile in the same directory.
// Run:   ./bench
//
// Notes on benchmarking discipline:
// - Each measurement is cumulative wall-clock of an inner loop divided by
//   iteration count, repeated SAMPLES times to extract median+p95.
// - `static volatile` sinks defeat the compiler's "loop-invariant
//   computation hoisting" — the analog of Rust's `black_box`.
// - Same iter count (1e6) and sample count (25) as the Rust harness so
//   the JSON streams concatenate.

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <ctime>
#include <string>
#include <vector>

constexpr std::size_t kIter = 1'000'000;
constexpr std::size_t kSamples = 25;
constexpr const char* kHost = "M4-Pro-aarch64";
constexpr const char* kStone = "kevy-bytes";
constexpr const char* kLanguage = "cpp";

static volatile std::uint64_t g_sink_u64 = 0;
static volatile bool g_sink_bool = false;

[[gnu::noinline]] static void sink_string(const std::string& s) {
    g_sink_u64 ^= reinterpret_cast<std::uintptr_t>(s.data());
    g_sink_u64 ^= s.size();
}

[[gnu::noinline]] static void sink_bool(bool b) { g_sink_bool ^= b; }

static std::string iso_utc_now() {
    auto t = std::time(nullptr);
    char buf[32];
    std::strftime(buf, sizeof(buf), "%Y-%m-%dT%H:%M:%SZ", std::gmtime(&t));
    return buf;
}

template <typename F>
static std::uint64_t time_one(std::size_t iter, F&& f) {
    auto t0 = std::chrono::steady_clock::now();
    for (std::size_t i = 0; i < iter; ++i) {
        f();
    }
    auto t1 = std::chrono::steady_clock::now();
    auto ns =
        std::chrono::duration_cast<std::chrono::nanoseconds>(t1 - t0).count();
    return static_cast<std::uint64_t>(ns) / iter;
}

template <typename F>
static void bench(const char* competitor, const char* workload, F&& f) {
    std::vector<std::uint64_t> times;
    times.reserve(kSamples);
    for (std::size_t i = 0; i < kSamples; ++i) {
        times.push_back(time_one(kIter, f));
    }
    std::sort(times.begin(), times.end());
    auto med = times[kSamples / 2];
    auto p95 = times[(kSamples * 95) / 100];
    auto min = times[0];
    auto date = iso_utc_now();
    std::printf(
        "{\"stone\":\"%s\",\"language\":\"%s\",\"competitor\":\"%s\","
        "\"workload\":\"%s\",\"metric\":\"ns_per_op\","
        "\"value_median\":%llu,\"value_p95\":%llu,\"value_min\":%llu,"
        "\"iterations\":%zu,\"host\":\"%s\",\"date\":\"%s\"}\n",
        kStone, kLanguage, competitor, workload,
        static_cast<unsigned long long>(med),
        static_cast<unsigned long long>(p95),
        static_cast<unsigned long long>(min), kIter, kHost, date.c_str());
}

int main() {
    const std::string short_str = "hello world!";  // 12 bytes — inline
    const std::string long_str(64, 'a');            // 64 bytes — heap

    // ---- clone ----
    {
        std::string src = short_str;
        bench("std::string", "clone_inline_12B", [&]() {
            std::string c = src;
            sink_string(c);
        });
    }
    {
        std::string src = long_str;
        bench("std::string", "clone_heap_64B", [&]() {
            std::string c = src;
            sink_string(c);
        });
    }

    // ---- eq ----
    {
        std::string a = short_str;
        std::string b = short_str;
        bench("std::string", "eq_inline_12B", [&]() {
            const std::string* aa = &a;
            const std::string* bb = &b;
            asm volatile("" : "+r"(aa));
            asm volatile("" : "+r"(bb));
            sink_bool(*aa == *bb);
        });
    }
    {
        std::string a = long_str;
        std::string b = long_str;
        bench("std::string", "eq_heap_64B", [&]() {
            const std::string* aa = &a;
            const std::string* bb = &b;
            asm volatile("" : "+r"(aa));
            asm volatile("" : "+r"(bb));
            sink_bool(*aa == *bb);
        });
    }

    // ---- from_str ----
    bench("std::string", "from_str_inline_12B", [&]() {
        const char* p = short_str.c_str();
        asm volatile("" : "+r"(p));
        std::string s(p);
        sink_string(s);
    });
    bench("std::string", "from_str_heap_64B", [&]() {
        const char* p = long_str.c_str();
        asm volatile("" : "+r"(p));
        std::string s(p, 64);
        sink_string(s);
    });

    // Use the sinks so they aren't optimised away entirely.
    if (g_sink_u64 == 0xDEADBEEF) std::printf("// sink_u64\n");
    if (g_sink_bool) std::printf("// sink_bool\n");
    return 0;
}
