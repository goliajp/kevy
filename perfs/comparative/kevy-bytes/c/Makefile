# Build the SDS competitor bench. SDS is Redis' byte-string library
# (antirez/sds; standalone version vendored alongside).

CC      ?= clang
CFLAGS  ?= -O3 -std=c11 -Wall -DNDEBUG

bench: main.c sds.c sds.h sdsalloc.h
	$(CC) $(CFLAGS) -o $@ main.c sds.c

clean:
	rm -f bench

.PHONY: clean
