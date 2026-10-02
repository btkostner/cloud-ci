-- GitHub App installation discovery and allowlisting
-- (docs/design/auth.md § "Multiple orgs and installations", "Data model";
-- docs/architecture.md § "Data model"). Forward-only, per AGENTS.md.
--
-- Unlike 0001/0002 (RunCoordinator's own D1 projection), these tables are
-- written directly by the `/webhooks/github` route in src/lib.rs: org/repo
-- discovery has no designated Durable Object owner, and AGENTS.md's "only a
-- run's RunCoordinator writes run state" invariant is scoped to the
-- runs/jobs/job_shards/uploads/reports tables (0001/0002), not this
-- unrelated domain.

CREATE TABLE installations (
    installation_id INTEGER PRIMARY KEY,
    account_login TEXT NOT NULL,
    account_type TEXT NOT NULL,
    suspended_at INTEGER,
    installed_at INTEGER NOT NULL
);

-- Owned by architecture.md's core data model (repo_id, installation_id,
-- name), per docs/design/auth.md's "Data scoping" note: `repo_id` only
-- exists once per (installation_id, repo) pair, so every per-repo row
-- elsewhere is implicitly per-installation by joining through repo_id.
CREATE TABLE repos (
    repo_id INTEGER PRIMARY KEY,
    installation_id INTEGER NOT NULL REFERENCES installations (installation_id),
    name TEXT NOT NULL
);
