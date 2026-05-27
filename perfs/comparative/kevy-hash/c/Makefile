CC      ?= clang
CFLAGS  ?= -O3 -std=c11 -Wall -DNDEBUG

bench: main.c xxhash.h wyhash.h
	$(CC) $(CFLAGS) -o $@ main.c

clean:
	rm -f bench

.PHONY: clean
