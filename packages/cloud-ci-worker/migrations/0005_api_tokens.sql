-- Scoped API tokens (docs/design/auth.md § "Scoped API tokens", "Data
-- model"). Forward-only, per AGENTS.md.
--
-- Opaque, caller-presented tokens of the form `cc_tok_<32 random bytes,
-- base64url>`; only `sha256(token)` is ever stored, in `token_hash`, so a
-- leaked D1 export does not yield a usable credential (same reasoning as
-- `sessions.id` would use once human login exists).
--
-- `created_by` is TEXT, not `INTEGER REFERENCES users (id)`, and is
-- nullable: the `users` table (human GitHub OAuth login) does not exist
-- yet — that is a separate, much larger piece of work. A real admin-issued
-- token will carry a real `created_by` once human login exists; until
-- then this column is either NULL or a free-text label (e.g. the
-- operator's own note to themselves), never a foreign key to a table
-- that doesn't exist.
--
-- Like installations/repos (0003), this table has no designated Durable
-- Object owner — it is written directly by whatever issues/looks up
-- tokens, not through RunCoordinator's projection.
CREATE TABLE api_tokens (
    id TEXT PRIMARY KEY, -- ULID
    token_hash BLOB NOT NULL, -- sha256(plaintext token), 32 bytes
    name TEXT NOT NULL,
    scopes TEXT NOT NULL, -- JSON array, e.g. ["ingest:write"]
    repo_allowlist TEXT, -- JSON array of repo_id, NULL = all repos
    created_by TEXT, -- free-text label; no `users` table yet, see above
    created_at INTEGER NOT NULL,
    expires_at INTEGER, -- NULL = never expires
    last_used_at INTEGER, -- best-effort, updated on successful verification
    revoked_at INTEGER -- NULL = not revoked
);

-- Every verification does `SELECT ... WHERE token_hash = ?1`, never a scan.
CREATE UNIQUE INDEX api_tokens_token_hash ON api_tokens (token_hash);

-- Local development / testing only. There is no issuance code in this
-- deployment yet (`cloud-ci login` and the admin-only `POST /v1/tokens`
-- dashboard endpoint, per docs/design/auth.md § "Scoped API tokens", are
-- both unbuilt), and this round deliberately does not add a third,
-- informal issuance path in Rust — minting a token is a privilege-
-- granting operation and should only ever exist behind a real, reviewed
-- issuance surface. To hand-craft a usable token against your own local
-- `wrangler dev` D1 instance for testing:
--
--   # 1. Generate 32 random bytes and base64url-encode them (no padding).
--   RAND=$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n')
--   TOKEN="cc_tok_${RAND}"
--   echo "plaintext token (shown once): ${TOKEN}"
--
--   # 2. SHA-256 the *whole token string* (including the cc_tok_ prefix),
--   #    and format the digest as a SQLite BLOB literal.
--   HASH_HEX=$(printf '%s' "${TOKEN}" | openssl dgst -sha256 -binary | xxd -p -c 256)
--   echo "token_hash: x'${HASH_HEX}'"
--
--   # 3. Insert the row directly (adjust scopes/repo_allowlist/name as needed).
--   wrangler d1 execute DB --local --command \
--     "INSERT INTO api_tokens (id, token_hash, name, scopes, repo_allowlist, created_by, created_at, expires_at, last_used_at, revoked_at) \
--      VALUES ('01HQZZZZZZZZZZZZZZZZZZZZZZ', x'${HASH_HEX}', 'dev-test-token', '[\"ingest:write\"]', '[12345]', 'local-dev', strftime('%s','now'), NULL, NULL, NULL)"
--
-- The resulting ${TOKEN} is then usable as `Authorization: Bearer ${TOKEN}`
-- against the locally running Worker. This is no different in kind from
-- an operator running any other raw SQL against their own local dev
-- database — it ships no Rust code capable of minting a token.
