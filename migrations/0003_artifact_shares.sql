-- Save backups shared with other users. Recipient ids are official Hydra
-- user ids (the launcher offers the user's friend list when sharing), so a
-- recipient may not exist in `users` yet — the share becomes visible once
-- they sign in to this server.
CREATE TABLE artifact_shares (
    id TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    owner_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    recipient_user_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (artifact_id, recipient_user_id)
);

CREATE INDEX idx_artifact_shares_recipient ON artifact_shares(recipient_user_id);
CREATE INDEX idx_artifact_shares_artifact ON artifact_shares(artifact_id);
