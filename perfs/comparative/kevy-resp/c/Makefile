# hiredis reference bench. Build: `make` produces ./bench.
# Requires `hiredis` (mac: brew install hiredis; Debian: apt-get install libhiredis-dev).

CC ?= gcc
CFLAGS ?= -O3 -Wall -Wextra
# Pick up brew prefix on Apple Silicon; Debian's apt headers/libs are on the
# default search path so the explicit -I/-L are no-ops there.
HIREDIS_PREFIX := $(shell brew --prefix hiredis 2>/dev/null)
ifdef HIREDIS_PREFIX
CFLAGS  += -I$(HIREDIS_PREFIX)/include
LDFLAGS += -L$(HIREDIS_PREFIX)/lib
endif
LDFLAGS += -lhiredis

bench: bench.c
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

clean:
	rm -f bench

.PHONY: clean
