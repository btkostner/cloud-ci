-- Human auth: GitHub OAuth login, D1 sessions, and the per-repo role
-- lookup cache (docs/design/auth.md § "Human auth: GitHub OAuth", "Role
-- resolution", "Data model"). Forward-only, per AGENTS.md.
--
-- Like installations/repos (0003) and api_tokens (0005), these tables
-- have no designated Durable Object owner — they are written directly by
-- the OAuth callback (`src/oauth.rs`) and role-resolution read-through
-- cache (`src/roles.rs`), not through RunCoordinator's projection.

-- One row per human GitHub identity that has ever completed OAuth login.
-- `github_user_id` is the numeric GitHub account id (immutable across
-- login renames, same reasoning as `installations.account_id`), unique so
-- the OAuth callback can upsert on it.
CREATE TABLE users (
    id TEXT PRIMARY KEY, -- ULID
    github_user_id INTEGER NOT NULL UNIQUE,
    github_login TEXT NOT NULL,
    email TEXT,
    created_at INTEGER NOT NULL,
    last_login_at INTEGER NOT NULL
);

-- Session storage is D1 rows, not stateless signed cookies (auth.md §
-- "Alternatives considered": revocation would need a deny-list anyway).
-- `id` is the lowercase hex SHA-256 digest of the random session id the
-- `__Host-cc_session` cookie carries — never the plaintext id itself, so
-- a leaked D1 export does not yield a usable session (auth.md's "Security
-- considerations" row on session cookie theft).
CREATE TABLE sessions (
    id TEXT PRIMARY KEY, -- sha256(cookie value), hex
    user_id TEXT NOT NULL REFERENCES users (id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

-- Every session lookup needs the owning user's row (github_login for the
-- role-resolution call); this index backs `DELETE FROM sessions WHERE
-- user_id = ?` for admin-triggered revoke of every session belonging to
-- one user, not just a scan.
CREATE INDEX sessions_user_id ON sessions (user_id);

-- `GET /repos/{owner}/{repo}/collaborators/{username}/permission` results,
-- cached with a 5-minute TTL enforced at read time (auth.md § "Role
-- resolution": "TTL enforced at read time (`checked_at` + 5 min), not by
-- a cron sweep"). `repo_id` implies installation via the `repos` table
-- (0003), so no `installation_id` column is needed here (auth.md's "Data
-- scoping" note).
CREATE TABLE repo_role_cache (
    user_id TEXT NOT NULL REFERENCES users (id),
    repo_id INTEGER NOT NULL REFERENCES repos (repo_id),
    role TEXT NOT NULL, -- "viewer" | "operator" | "admin"
    checked_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, repo_id)
);
