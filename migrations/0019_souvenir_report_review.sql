-- What the operator did about a report, so the Reports screen can tell what
-- still needs a look from what was already handled. Every report is open
-- until then: 'dismissed' (nothing wrong), 'hidden' (the picture was taken
-- off the profile) or 'removed' (the picture was deleted).
ALTER TABLE souvenir_reports ADD COLUMN status TEXT NOT NULL DEFAULT 'open';
ALTER TABLE souvenir_reports ADD COLUMN resolved_at TEXT;

CREATE INDEX idx_souvenir_reports_status ON souvenir_reports (status, created_at DESC);
