-- v2.7: make the long-dormant qa_findings table the record of *why* a segment
-- was flagged. The table itself dates back to 0001_initial.sql but nothing ever
-- wrote to it, so free-text `segments.error` was the only evidence available.
--
-- Only the read indexes are expressible here. The row backfill for jobs that
-- predate v2.7 runs in Rust (schema::backfill_qa_findings driving
-- bookforge_store::classify_segment_error) because splitting a concatenated
-- `segments.error` string into a taxonomy is not expressible in SQL.
CREATE INDEX IF NOT EXISTS idx_qa_findings_job ON qa_findings(job_id, kind);
CREATE INDEX IF NOT EXISTS idx_qa_findings_segment ON qa_findings(job_id, segment_id);
