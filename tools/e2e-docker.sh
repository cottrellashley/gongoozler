#!/bin/sh
# The whole box path, end to end, with Docker: a server and a worker, each in a container of the
# image, the worker running each job in a container of its own through the Docker socket; a run
# submitted from here, its events followed, its results fetched. Needs Docker, and bin/gongoozler.
#
#   ./tools/e2e-docker.sh [IMAGE]          (make e2e-docker builds the image first)
set -eu
image=${1:-gongoozler}
net=gongoozler-e2e-$$
token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
port=${GONGOOZLER_E2E_PORT:-18150}
state=$(mktemp -d)
cleanup() {
    docker rm -f "$net-server" "$net-worker" >/dev/null 2>&1 || true
    docker ps -aq --filter label=gongoozler.run | xargs -r docker rm -f >/dev/null 2>&1 || true
    docker network rm "$net" >/dev/null 2>&1 || true
    rm -rf "$state"
}
trap cleanup EXIT INT TERM

docker network create "$net" >/dev/null
docker run -d --name "$net-server" --network "$net" -p "127.0.0.1:$port:8150" -e GONGOOZLER_TOKEN="$token" \
    "$image" server --listen 0.0.0.0:8150 --data /state/server >/dev/null
# The worker reaches the daemon through its socket, so it runs as the socket's group.
docker run -d --name "$net-worker" --network "$net" -v /var/run/docker.sock:/var/run/docker.sock \
    --group-add "$(stat -c %g /var/run/docker.sock)" -e GONGOOZLER_TOKEN="$token" \
    "$image" worker --server "http://$net-server:8150" --docker --image "$image" --network "$net" --memory 4GiB --cpus 2 >/dev/null

for _ in $(seq 50); do
    curl -fsS -H "Authorization: Bearer $token" "http://127.0.0.1:$port/v1/health" >/dev/null 2>&1 && break
    sleep 0.2
done

export GONGOOZLER_STATE="$state" GONGOOZLER_TOKEN="$token" GONGOOZLER_SERVER="http://127.0.0.1:$port"
./bin/gongoozler run --remote --plain --no-lock --param hours=6 examples/in_a_box.py
id=$(./bin/gongoozler ls --json | sed -n 's/.*"id": "\([^"]*\)".*/\1/p' | tail -1)
for f in manifest.json orders.jsonl stats.json series.jsonl; do
    test -s "$state/runs/$id/artifacts/$f" || { echo "e2e: no $f from the box"; exit 1; }
done
echo "e2e: run $id ran in a box and its results came back"
docker logs "$net-worker" 2>&1 | tail -5
