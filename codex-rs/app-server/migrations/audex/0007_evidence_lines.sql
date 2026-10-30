-- Legacy claims remain explicitly unverified; only new source-checked commits set this.
ALTER TABLE evidence ADD COLUMN lines_verified INTEGER NOT NULL DEFAULT 0 CHECK (lines_verified IN (0,1));
