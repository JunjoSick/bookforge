-- v2.4: durable, auditable human translation overrides.
ALTER TABLE translations ADD COLUMN origin TEXT NOT NULL DEFAULT 'model';
ALTER TABLE translations ADD COLUMN human_corrected INTEGER NOT NULL DEFAULT 0;
ALTER TABLE translations ADD COLUMN corrected_at TEXT;

