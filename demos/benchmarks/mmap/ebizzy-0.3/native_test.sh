#! /bin/bash
make

THREAD_MAX=8
for j in $(eval echo "{1..$THREAD_MAX}")
do
	(( result=0 ))
	for i in {0..2}
	do
		(( ret_$i=$(./ebizzy -vv -T -R -l -t $j -m 2>&1 | grep "records/s" | awk '{print $1}') ))
		(( result = ret_$i + result ))
	done
	(( ret = result / 3))
	echo "Thread = $j, performance: $ret records/s"
done
