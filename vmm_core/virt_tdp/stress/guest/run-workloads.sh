#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -uo pipefail
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
seconds=60
for arg in $(cat /proc/cmdline); do
    case "$arg" in virt_tdp_stress_seconds=*) seconds=${arg#*=};; esac
done
[[ $seconds =~ ^[1-9][0-9]*$ ]] || seconds=60
cpus=$(nproc)
root=/var/log/virt-tdp-stress
mkdir -p "$root"
pass=0 fail=0 total=0

emit() { printf '<3>VIRT_TDP_STRESS %s\n' "$*" >/dev/kmsg; }
metric() {
    local name=$1 file=$2 line
    line=$(tail -n 8 "$file" | tr '\n' ' ' | sed 's/[^A-Za-z0-9_.:,+%\/= -]/_/g;s/  */_/g' | cut -c1-700)
    emit "METRIC name=$name text=${line:-none}"
}
run_case() {
    local name=$1; shift
    local output="$root/$name.log" start end rc
    total=$((total + 1)); start=$(date +%s%N)
    timeout --kill-after=10 "${CASE_TIMEOUT:-600}" bash -c '"$@"' _ "$@" >"$output" 2>&1
    rc=$?; end=$(date +%s%N)
    if ((rc == 0)); then
        pass=$((pass + 1)); emit "RESULT name=$name status=PASS duration_ms=$(((end-start)/1000000))"
    else
        fail=$((fail + 1)); emit "RESULT name=$name status=FAIL duration_ms=$(((end-start)/1000000)) reason=exit_$rc"
    fi
    metric "$name" "$output"
    return 0
}

load_images() { gzip -dc /opt/virt-tdp-stress/images.tar.gz | docker load; }
docker_info() { docker info; docker image ls; }
cpu_parallel() { docker run --rm --network none virt-tdp-workload:latest cpu "$cpus" "$seconds"; }
memory_bandwidth() { docker run --rm --network none --memory 2g virt-tdp-workload:latest memory "$cpus" 1024 12; }
process_churn() { docker run --rm --network none --pids-limit 4096 virt-tdp-workload:latest process 5000; }
filesystem_sequential() {
    mkdir -p /var/lib/virt-tdp/io
    docker run --rm --network none -v /var/lib/virt-tdp/io:/data postgres:15-alpine sh -ec '
      dd if=/dev/urandom of=/data/payload bs=1M count=512 conv=fsync
      sha256sum /data/payload > /data/payload.sha256
      dd if=/data/payload of=/dev/null bs=4M
      cd /data && sha256sum -c payload.sha256
    '
}
start_postgres() {
    mkdir -p /var/lib/virt-tdp/postgres; chmod 0777 /var/lib/virt-tdp/postgres
    docker rm -f pg >/dev/null 2>&1 || true
    docker run -d --name pg --network host -e POSTGRES_HOST_AUTH_METHOD=trust \
      -e PGDATA=/var/lib/postgresql/data/pgdata \
      -v /var/lib/virt-tdp/postgres:/var/lib/postgresql/data postgres:15-alpine
    for _ in $(seq 1 120); do docker exec pg pg_isready >/dev/null 2>&1 && return 0; sleep 1; done
    return 1
}
postgres_pgbench() {
    docker rm -f pg >/dev/null 2>&1 || true
    rm -rf /var/lib/virt-tdp/postgres
    start_postgres || return
    docker exec pg createdb -U postgres bench
    docker exec pg pgbench -U postgres -i -s 10 bench
    docker exec pg pgbench -U postgres -c "$((cpus * 2))" -j "$cpus" -T "$seconds" -P 10 bench
}
postgres_persistence() {
    before=$(docker exec pg psql -U postgres -At bench -c 'select count(*) from pgbench_accounts')
    docker stop -t 30 pg >/dev/null; docker rm pg >/dev/null
    start_postgres || return
    after=$(docker exec pg psql -U postgres -At bench -c 'select count(*) from pgbench_accounts')
    [[ $before = 1000000 && $after = "$before" ]]
    docker exec pg psql -U postgres -At bench -c 'select pg_database_size(current_database())'
}
start_redis() {
    mkdir -p /var/lib/virt-tdp/redis; chmod 0777 /var/lib/virt-tdp/redis
    docker rm -f redis >/dev/null 2>&1 || true
    docker run -d --name redis --network host -v /var/lib/virt-tdp/redis:/data \
      redis:7-alpine redis-server --appendonly yes --appendfsync everysec
    for _ in $(seq 1 60); do docker exec redis redis-cli ping 2>/dev/null | grep -q PONG && return 0; sleep 1; done
    return 1
}
redis_benchmark() {
    docker rm -f redis >/dev/null 2>&1 || true
    rm -rf /var/lib/virt-tdp/redis
    start_redis || return
    docker exec redis redis-benchmark -q -n 300000 -c 64 -P 16 -t set,get
}
redis_persistence() {
    docker exec redis redis-cli set virt-tdp-persistent survived | grep -q OK
    docker exec redis redis-cli wait 0 1000 >/dev/null
    docker stop -t 30 redis >/dev/null; docker rm redis >/dev/null
    start_redis || return
    docker exec redis redis-cli get virt-tdp-persistent | grep -q survived
}
network_raw_tx() {
    netif=$(find /sys/class/net -mindepth 1 -maxdepth 1 -printf '%f\n' | grep -v '^lo$' | head -n 1)
    [[ -n $netif ]]
    ip link set "$netif" up
    docker run --rm --network host --cap-add NET_RAW virt-tdp-workload:latest network "$netif" 500000 1500
}
mixed_stress() {
    local cpu_pid memory_pid redis_pid pg_rc=0 cpu_rc=0 memory_rc=0 redis_rc=0
    docker run --rm --name mixed-cpu --network none virt-tdp-workload:latest cpu "$cpus" "$seconds" >"$root/mixed-cpu.log" 2>&1 & cpu_pid=$!
    docker run --rm --name mixed-memory --network none --memory 2g virt-tdp-workload:latest memory "$cpus" 1024 40 >"$root/mixed-memory.log" 2>&1 & memory_pid=$!
    docker exec redis redis-benchmark -q -n 500000 -c 64 -P 16 -t set,get >"$root/mixed-redis.log" 2>&1 & redis_pid=$!
    docker exec pg pgbench -U postgres -c "$((cpus * 2))" -j "$cpus" -T "$seconds" -P 10 bench || pg_rc=$?
    wait "$cpu_pid" || cpu_rc=$?
    wait "$memory_pid" || memory_rc=$?
    wait "$redis_pid" || redis_rc=$?
    cat "$root/mixed-cpu.log" "$root/mixed-memory.log" "$root/mixed-redis.log"
    ((pg_rc == 0 && cpu_rc == 0 && memory_rc == 0 && redis_rc == 0))
}

export seconds cpus root
export -f load_images docker_info cpu_parallel memory_bandwidth process_churn
export -f filesystem_sequential start_postgres postgres_pgbench postgres_persistence
export -f start_redis redis_benchmark redis_persistence network_raw_tx mixed_stress

emit "BEGIN version=1 cpus=$cpus duration_seconds=$seconds os=ubuntu docker=true"
for _ in $(seq 1 120); do docker info >/dev/null 2>&1 && break; sleep 1; done
run_case docker.load load_images
run_case docker.info docker_info
run_case cpu.parallel cpu_parallel
run_case memory.bandwidth memory_bandwidth
run_case process.churn process_churn
run_case filesystem.sequential filesystem_sequential
run_case postgres.pgbench postgres_pgbench
run_case postgres.persistence postgres_persistence
run_case redis.benchmark redis_benchmark
run_case redis.persistence redis_persistence
run_case network.raw_tx network_raw_tx
CASE_TIMEOUT=$((seconds + 300)) run_case mixed.database_cpu_memory mixed_stress
sync
emit "END total=$total pass=$pass fail=$fail"
# Leave the machine inspectable. The host runner requests a clean OpenVMM stop.
sleep infinity
