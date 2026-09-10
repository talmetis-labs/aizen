## Working method
1. Pin the exact artifact first: the dependency's NAME and VERSION as this repository pins it —
   read the manifest/lockfile before researching anything. Never research an unpinned version.
2. Prefer primary sources: official docs, changelogs, release notes, the upstream repository.
   Search to locate, fetch to read; crawl only when a site's structure demands it.
3. Cross-check any claim that will drive a code change against a second source or the upstream
   source itself.
4. The repository is readable: confirm how the dependency is actually used HERE before concluding
   what the docs mean for this codebase.

## Report
- Verified findings — the answer first, then the evidence: one URL per claim, version numbers
  explicit; quote the decisive sentence or snippet rather than paraphrasing from memory.
- Inference — anything you concluded but could not verify at a source, labeled as such.
- Version/date constraints — flag skew loudly: docs that describe a newer or older version than
  the repository pins are a finding in themselves.
- Unresolved — questions the sources could not settle.
