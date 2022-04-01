#!/bin/bash
set -e

# 1. Init Occlum Workspace
rm -rf occlum_instance && occlum new occlum_instance
cd occlum_instance

# 2. Copy files into Occlum Workspace and build
rm -rf image
copy_bom -f ../rocksdb.yaml --root image --include-dir /opt/occlum/etc/template

new_json="$(jq '.resource_limits.user_space_size = "3000MB" |
                .resource_limits.kernel_space_heap_size ="1024MB" |
                .resource_limits.kernel_space_stack_size ="4MB" |
                .process.default_heap_size = "128MB" |
                .process.default_mmap_size = "2000MB" |
                .resource_limits.max_num_of_threads = 96' Occlum.json)" && \
echo "${new_json}" > Occlum.json

occlum build

# 3. Run example and benchmark with config
BLUE='\033[1;34m'
NC='\033[0m'
echo -e "${BLUE}Run simple_rocksdb_example in Occlum.${NC}"
occlum run /bin/simple_rocksdb_example

echo -e "${BLUE}Run benchmark in Occlum.${NC}"

BENCHMARK_CONFIG="fillseq,fillrandom,readseq,readrandom,deleteseq"
occlum run /bin/db_bench --benchmarks=${BENCHMARK_CONFIG}

:<<!
Benchmark config list:
        fillseq       -- write N values in sequential key order in async mode
      	fillseqdeterministic       -- write N values in the specified key order and keep the shape of the LSM tree
      	fillrandom    -- write N values in random key order in async mode
      	filluniquerandomdeterministic       -- write N values in a random key order and keep the shape of the LSM tree
      	overwrite     -- overwrite N values in random key order in async mode
      	fillsync      -- write N/100 values in random key order in sync mode
      	fill100K      -- write N/1000 100K values in random order in async mode
      	deleteseq     -- delete N keys in sequential order
      	deleterandom  -- delete N keys in random order
      	readseq       -- read N times sequentially
      	readtocache   -- 1 thread reading database sequentially
      	readreverse   -- read N times in reverse order
      	readrandom    -- read N times in random order
      	readmissing   -- read N missing keys in random order
      	readwhilewriting      -- 1 writer, N threads doing random reads
      	readwhilemerging      -- 1 merger, N threads doing random reads
      	readrandomwriterandom -- N threads doing random-read, random-write
      	prefixscanrandom      -- prefix scan N times in random order
      	updaterandom  -- N threads doing read-modify-write for random keys
      	appendrandom  -- N threads doing read-modify-write with growing values
      	mergerandom   -- same as updaterandom/appendrandom using merge operator. Must be used with merge_operator
      	readrandommergerandom -- perform N random read-or-merge operations. Must be used with merge_operator
      	newiterator   -- repeated iterator creation
      	seekrandom    -- N random seeks, call Next seek_nexts times per seek
      	seekrandomwhilewriting -- seekrandom and 1 thread doing overwrite
      	seekrandomwhilemerging -- seekrandom and 1 thread doing merge
      	crc32c        -- repeated crc32c of 4K of data
      	xxhash        -- repeated xxHash of 4K of data
      	acquireload   -- load N*1000 times
      	fillseekseq   -- write N values in sequential key, then read them by seeking to each key
      	randomtransaction     -- execute N random transactions and verify correctness
      	randomreplacekeys     -- randomly replaces N keys by deleting the old version and putting the new version
        timeseries            -- 1 writer generates time series data and multiple readers doing random reads on id
More ref at https://github.com/facebook/rocksdb/wiki/Benchmarking-tools
!

echo -e "${BLUE}Run benchmark in host.${NC}"
cd ../rocksdb && ./db_bench --benchmarks=$BENCHMARK_CONFIG
