-- Fuzzy title search (typos, partial words). pg_trgm is a trusted extension, so the
-- database owner can create it (on Supabase too). Whether it handles Thai depends on the
-- database's LC_CTYPE: under "C" Thai yields no trigrams, so search also matches
-- substrings with ILIKE, which works in any locale.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Serves both the ILIKE substring match and the trigram similarity operators.
CREATE INDEX library_tracks_title_trgm ON library_tracks USING gin (lower(title) gin_trgm_ops);
