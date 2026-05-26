# Build the std::string competitor bench. Apple Clang on macOS aarch64
# defaults to libc++ (22-byte SSO). On linux, GCC defaults to libstdc++
# (15-byte SSO); set CXXSTDLIB=libc++ if you want the libc++ flavour.

CXX        ?= clang++
CXXSTDLIB  ?= libc++
CXXFLAGS   ?= -O3 -std=c++20 -fno-omit-frame-pointer -DNDEBUG
LDFLAGS    ?= -stdlib=$(CXXSTDLIB)

bench: main.cpp
	$(CXX) $(CXXFLAGS) $(LDFLAGS) -o $@ $<

clean:
	rm -f bench

.PHONY: clean
