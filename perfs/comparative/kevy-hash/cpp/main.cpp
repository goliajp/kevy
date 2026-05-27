// Cross-language counterpart of kevy-hash's bench, C++ side:
// std::hash<std::string_view> (the default char-string hashtable hash
// for libc++ / libstdc++; usually MurmurHash2 variants) on the same
// hash_bytes / hash_u64 workloads.

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <ctime>
#include <functional>
#include <string_view>
#include <vector>

constexpr std::size_t kIter = 1'000'000;
constexpr std::size_t kSamples = 25;
constexpr const char* kHost = "M4-Pro-aarch64";
constexpr const char* kStone = "kevy-hash";
constexpr const char* kLang = "cpp";

static volatile std::uint64_t g_sink = 0;
[[gnu::noinline]] static void sink_u64(std::uint64_t x) { g_sink ^= x; }

static std::string iso_utc_now() {
    auto t = std::time(nullptr);
    char buf[32];
    std::strftime(buf, sizeof(buf), "%Y-%m-%dT%H:%M:%SZ", std::gmtime(&t));
    return buf;
}

template <typename F>
static std::uint64_t time_one(F&& f) {
    auto t0 = std::chrono::steady_clock::now();
    std::uint64_t acc = 0;
    for (std::size_t i = 0; i < kIter; ++i) acc ^= f();
    auto t1 = std::chrono::steady_clock::now();
    sink_u64(acc);
    auto ns =
        std::chrono::duration_cast<std::chrono::nanoseconds>(t1 - t0).count();
    return static_cast<std::uint64_t>(ns) / kIter;
}

template <typename F>
static void bench(const char* competitor, const char* workload, F&& f) {
    std::vector<std::uint64_t> times;
    times.reserve(kSamples);
    for (std::size_t i = 0; i < kSamples; ++i) times.push_back(time_one(f));
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
        kStone, kLang, competitor, workload,
        static_cast<unsigned long long>(med),
        static_cast<unsigned long long>(p95),
        static_cast<unsigned long long>(min), kIter, kHost, date.c_str());
}

int main() {
    std::hash<std::string_view> sv_hash;
    std::hash<std::uint64_t> u64_hash;

    const std::string s8(8, '\x01');
    const std::string s16(16, '\x02');
    const std::string s64(64, '\x03');

    bench("std::hash<string_view>", "hash_bytes_8B",
          [&]() { return (std::uint64_t)sv_hash(std::string_view(s8)); });
    bench("std::hash<string_view>", "hash_bytes_16B",
          [&]() { return (std::uint64_t)sv_hash(std::string_view(s16)); });
    bench("std::hash<string_view>", "hash_bytes_64B",
          [&]() { return (std::uint64_t)sv_hash(std::string_view(s64)); });

    bench("std::hash<uint64_t>", "hash_u64", [&]() {
        std::uint64_t n = 0xdeadbeefcafebabeULL;
        asm volatile("" : "+r"(n));
        return (std::uint64_t)u64_hash(n);
    });

    if (g_sink == 0xDEADBEEF) std::printf("// sink\n");
    return 0;
}
