# `durable_object`-scheduling-policy image for the "Container snapshots"
# Phase-0 spike (docs/roadmap.md), driven by
# `packages/cloud-ci-worker/src/container_snapshot_spike.rs`. Not the
# repo's eventual real Containers execution image (there is no
# `cloud-ci-runner-image` package yet, and this spike does not create
# one) — same Phase-0 proof-fixture spirit as `container_probe/Dockerfile`,
# but with `pnpm` available and a real, non-trivial `package.json` baked
# in (not installed) so the container's own "setup" step
# (`pnpm install`, exec'd live by the DO, not run at build time) has real,
# measurable work to do.
FROM node:24.20.0-trixie-slim
RUN corepack enable && corepack prepare pnpm@9.15.0 --activate
WORKDIR /app
COPY package.json /app/package.json
ENTRYPOINT ["sleep", "infinity"]
