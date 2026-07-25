// clientgate: go-redis against a kevy server.
package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/redis/go-redis/v9"
)

func check(ok bool, what string) {
	if !ok {
		fmt.Fprintf(os.Stderr, "FAIL %s\n", what)
		os.Exit(1)
	}
}

func main() {
	ctx := context.Background()
	c := redis.NewClient(&redis.Options{Addr: "127.0.0.1:" + os.Getenv("KEVY_PORT")})
	defer c.Close()

	check(c.FlushAll(ctx).Err() == nil, "FLUSHALL")
	check(c.Set(ctx, "k", "v", 0).Val() == "OK", "SET")
	check(c.Get(ctx, "k").Val() == "v", "GET")
	check(c.IncrBy(ctx, "n", 5).Val() == 5, "INCRBY")
	check(c.PExpire(ctx, "k", 30*time.Second).Err() == nil, "PEXPIRE")
	ttl := c.PTTL(ctx, "k").Val()
	check(ttl > 0 && ttl <= 30*time.Second, "PTTL")

	check(c.HSet(ctx, "h", "a", "1", "b", "2").Err() == nil, "HSET")
	check(c.HGetAll(ctx, "h").Val()["a"] == "1", "HGETALL")
	check(c.LPush(ctx, "l", "x", "y").Err() == nil, "LPUSH")
	lr := c.LRange(ctx, "l", 0, -1).Val()
	check(len(lr) == 2 && lr[0] == "y" && lr[1] == "x", "LRANGE")
	check(c.ZAdd(ctx, "z", redis.Z{Score: 1, Member: "one"}, redis.Z{Score: 2, Member: "two"}).Err() == nil, "ZADD")
	zr := c.ZRange(ctx, "z", 0, -1).Val()
	check(len(zr) == 2 && zr[0] == "one" && zr[1] == "two", "ZRANGE")

	// pub/sub round trip
	sub := c.Subscribe(ctx, "room")
	defer sub.Close()
	_, err := sub.Receive(ctx) // the subscribe ack
	check(err == nil, "subscribe")
	check(c.Publish(ctx, "room", "hi").Err() == nil, "PUBLISH")
	msg, err := sub.ReceiveMessage(ctx)
	check(err == nil && msg.Payload == "hi", "pubsub")

	// the extended verb surface through the raw channel
	idx, err := c.Do(ctx, "IDX.LIST").Result()
	check(err == nil, "IDX.LIST raw")
	_, isSlice := idx.([]interface{})
	check(isSlice, "IDX.LIST shape")

	fmt.Println("ok")
}
