-- Which of a user's library games are copies of the same title in different
-- stores (a Steam app and its Epic catalog item), as the official catalogue
-- links them. The launcher reads the link off the official profile and sends
-- its whole set after every library sync; this server can't work it out
-- itself. Used to answer the combined Steam/Epic achievement view.
CREATE TABLE game_links (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    shop TEXT NOT NULL,
    object_id TEXT NOT NULL,
    canonical_id TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (user_id, shop, object_id)
);

CREATE INDEX idx_game_links_canonical ON game_links (user_id, canonical_id);
