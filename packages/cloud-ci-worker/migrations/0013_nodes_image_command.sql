-- `nodes.image`/`nodes.command` (coordinator::mod's module docs' Nodes section):
-- `startNode`'s what-to-run fields, carried through to D1's projection of the DO's
-- authoritative `node` table the same way every other node column already is
-- (0010_nodes.sql). `command` is stored as a JSON array (matching `check_names`'
-- existing JSON-column convention on `jobs`), not a delimited string: a command's
-- own arguments can contain any byte, including whatever delimiter a plain string
-- join would pick.
ALTER TABLE nodes ADD COLUMN image TEXT NOT NULL DEFAULT '';
ALTER TABLE nodes ADD COLUMN command TEXT NOT NULL DEFAULT '[]';
