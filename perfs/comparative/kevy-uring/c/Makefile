# liburing reference bench. Build: `make` produces ./bench.
# Requires `liburing-dev` (Debian: apt-get install liburing-dev).

CC ?= gcc
CFLAGS ?= -O3 -Wall -Wextra
LDFLAGS ?= -luring

bench: bench.c
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

clean:
	rm -f bench

.PHONY: clean
