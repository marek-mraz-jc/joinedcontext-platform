-- An administrator's exclusion of a page or a document (T-3057, AG-113): it holds over every
-- later crawl, whatever the source's include and exclude patterns say; `included` stays what the
-- patterns decided, so the Portal can say which of the two left an item out.
ALTER TABLE pages ADD COLUMN excluded_by_admin boolean NOT NULL DEFAULT false;
ALTER TABLE documents ADD COLUMN excluded_by_admin boolean NOT NULL DEFAULT false;
