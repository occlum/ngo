#! /bin/bash

if [ ! -d occlum_instance ]; then
	occlum-gcc -o mmap main.c
	occlum new occlum_instance
	cp mmap occlum_instance/image/bin
	cp Occlum.json occlum_instance
	cd occlum_instance && occlum build
else
	occlum-gcc -o mmap main.c
	cp mmap occlum_instance/image/bin
	cd occlum_instance && occlum build -f
fi

occlum start

count=1

t1=$(date +%s%N)
for i in $(seq 1 $count)
do
	occlum exec /bin/mmap
done
t2=$(date +%s%N)
((t = ($t2 - $t1) / 1000000))
echo "Time for all test is $t ms for $count operations."
occlum stop
((t = t / $count))
echo "Time for mmap test is $t ms/op"

