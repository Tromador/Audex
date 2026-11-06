-- Existing raw artefacts and historical report selections remain unchanged.
ALTER TABLE reviewer_assignments ADD COLUMN result_extraction_json TEXT;
ALTER TABLE reviewer_assignments ADD COLUMN result_extraction_digest TEXT;
