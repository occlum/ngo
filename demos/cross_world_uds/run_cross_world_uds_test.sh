#! /bin/bash
set -e

BLUE='\033[1;34m'
NC='\033[0m'

make

./server both_untrusted &
./client both_untrusted &
echo -e "Test server and client both in host: ${BLUE}[Pass]${NC}"

rm -rf occlum_instance*
occlum new occlum_instance && cd occlum_instance

new_json="$(jq '.untrusted_unix_socks += [
    {"host": "/tmp/occlum/test.sock", "libos": "/root/test.sock"},
    {"host": "/tmp/root/", "libos": "/root/"},
    {"host": "../test.sock", "libos":"/tmp/test.sock" }]' Occlum.json)" && \
echo "${new_json}" > Occlum.json

mkdir -p /tmp/occlum
mkdir -p /tmp/root
cp ../client image/bin
cp ../server image/bin

occlum build
occlum start
occlum exec /bin/server trusted &
sleep 1
../client untrusted
echo -e "Test trusted server with untruted client: ${BLUE}[Pass]${NC}"

../server untrusted &
occlum exec /bin/client trusted
occlum stop
echo -e "Test untrusted server with trusted client: ${BLUE}[Pass]${NC}"

cd ..
cp -r occlum_instance occlum_instance2
cd occlum_instance
occlum run /bin/server both_trusted &
sleep 1

cd ../occlum_instance2
occlum run /bin/client both_trusted
echo -e "Test server and client in different Occlum instance: ${BLUE}[Pass]${NC}"
