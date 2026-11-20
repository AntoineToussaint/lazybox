#!/bin/sh
# Exercise only disposable containers and a volume dedicated to this run.
set -eu

image=${1:?usage: smoke.sh IMAGE}
volume=$(docker volume create)
runner=
cleanup() {
    if [ -n "$runner" ]; then
        docker rm -f "$runner" >/dev/null || true
    fi
    docker volume rm "$volume" >/dev/null
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

start() {
    runner=$(docker run --detach --init --mount "type=volume,src=$volume,dst=/var/lib/lazybox" "$image")
}

operator() {
    docker exec "$runner" lazybox "$@"
}

wait_for_runner() {
    for attempt in $(seq 1 30); do
        if docker exec "$runner" sh -c '
            exec lazybox --endpoint "$XDG_DATA_HOME/lazybox/state/endpoint.json" info
        ' >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    docker logs "$runner" >&2
    return 1
}

start
wait_for_runner
[ "$(operator link serve --probe)" = 'lazybox-link 1' ]
before=$(operator link owners ls --json)
docker stop "$runner" >/dev/null
docker rm "$runner" >/dev/null
runner=

start
wait_for_runner
after=$(operator link owners ls --json)
if [ "$before" != "$after" ]; then
    printf '%s\n' 'Owner-link identity changed after container replacement.' >&2
    exit 1
fi
printf '%s\n' 'Runner endpoint and owner-link identity survived container replacement.'
