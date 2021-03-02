#! /bin/bash
occlum-gcc -o mmap main.c
count=1
t1=$(date +%s%N)
for i in $(seq 1 $count)
do
	 ./mmap
done
t2=$(date +%s%N)
((t = ($t2 - $t1) / 1000000))
echo "Time for all test is $t ms for $count operations."
((t = t / $count))
echo "Time for mmap test is $t ms/op"
