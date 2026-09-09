# fixtures/analyzers/

Fixture files used by attic-analyzers unit and integration tests.

| File | Purpose |
|------|---------|
| empty.txt | Zero-byte file; GenericAnalyzer must produce no RetrievalUnits |
| single_line.txt | Single-line file; must produce exactly one RetrievalUnit |
| plain_text.txt | Multi-line plain text; must produce bounded RetrievalUnits with correct spans |
| exactly_500_lines.txt | File with exactly MAX_LINES_PER_CHUNK (500) lines; must produce one chunk |
| 501_lines.txt | File with 501 lines; must produce exactly two chunks |
