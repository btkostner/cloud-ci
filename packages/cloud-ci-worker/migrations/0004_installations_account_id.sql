-- Adds the numeric GitHub account id to `installations`
-- (docs/design/byo-ci.md § Auth: `BeginRun`'s OIDC path matches the
-- claimed `repository_owner_id` against "the GitHub App installation that
-- owns the target repo" — specifically the numeric id, not
-- `account_login`, because logins are mutable across renames/transfers
-- while the underlying GitHub account id is not).
--
-- Forward-only, per AGENTS.md: adds a column, does not alter
-- 0003_installations_and_repos.sql's existing ones. No backfill of
-- existing rows: this is a fresh local-dev-only database with no real
-- deployment yet.

ALTER TABLE installations ADD COLUMN account_id INTEGER NOT NULL DEFAULT 0;
