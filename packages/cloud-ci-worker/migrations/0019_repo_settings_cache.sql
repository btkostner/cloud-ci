-- `src/repo_settings.rs`'s immutable `(repo_id, head_sha)` cache.
-- Forward-only, per AGENTS.md.
--
-- `raw_text` stores the raw validated `settings.yml` text, not a
-- serialized `Settings`/`Diagnostic` — see `repo_settings.rs`'s module
-- doc comment ("Caching") for why: neither type derives serde, and
-- `cloud-ci-core` deliberately carries no serde dependency. A cache hit
-- re-parses `raw_text` (pure, cheap, no I/O).
--
-- `file_present = 0` means `settings.yml` did not exist at this sha
-- (deployment defaults apply, settings.md-documented); `raw_text` is
-- NULL in that case. A row is immutable once written: the same sha's
-- content never changes, so this table is never updated, only inserted
-- (`ON CONFLICT DO NOTHING`).
CREATE TABLE repo_settings_cache (
    repo_id      INTEGER NOT NULL,
    head_sha     TEXT NOT NULL,
    file_present INTEGER NOT NULL CHECK (file_present IN (0, 1)),
    raw_text     TEXT,
    cached_at    INTEGER NOT NULL,
    PRIMARY KEY (repo_id, head_sha),
    CHECK ((file_present = 0) = (raw_text IS NULL))
);
