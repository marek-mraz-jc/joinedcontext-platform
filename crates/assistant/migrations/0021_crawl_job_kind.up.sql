-- T-4749: a website's daily sitemap check reads only the pages its sitemap says are new or
-- changed since they were read; the scheduled full crawl stays as it is.
ALTER TABLE crawl_jobs ADD COLUMN kind text NOT NULL DEFAULT 'full' CHECK (kind IN ('full', 'sitemap'));
