CREATE TABLE hidden_games (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    shop TEXT NOT NULL,
    object_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(user_id, shop, object_id)
);
CREATE INDEX idx_hidden_games_user ON hidden_games (user_id);
