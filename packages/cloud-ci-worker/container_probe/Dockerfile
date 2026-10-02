# Trivial `default`-scheduling-policy image for `ContainerProbe`
# (packages/cloud-ci-worker/src/container_probe.rs): no application code of
# its own, just a long-running process so `Container::exec()` has a running
# container to start processes inside. Not the "real" Containers execution
# image (that belongs to a later, much larger round) — this is a Phase-0
# proof fixture only, same spirit as the Containers-from-Rust fork's own
# test/container-echo fixture.
FROM alpine:3.20
ENTRYPOINT ["sleep", "infinity"]
