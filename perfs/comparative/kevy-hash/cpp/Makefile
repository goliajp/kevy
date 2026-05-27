CXX        ?= clang++
CXXSTDLIB  ?= libc++
CXXFLAGS   ?= -O3 -std=c++20 -DNDEBUG
LDFLAGS    ?= -stdlib=$(CXXSTDLIB)

bench: main.cpp
	$(CXX) $(CXXFLAGS) $(LDFLAGS) -o $@ $<

clean:
	rm -f bench

.PHONY: clean
