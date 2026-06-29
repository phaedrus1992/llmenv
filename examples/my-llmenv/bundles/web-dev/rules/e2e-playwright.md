---
paths:
  - "**/e2e_tests/**"
  - "**/e2e/**"
  - "**/*.spec.ts"
  - "**/*.spec.js"
---

# E2E Test Rules (Playwright)

## Page Object Model (POM)

- **Never** put UI interactions or assertions directly in test files.
- All Playwright API calls belong in page objects.
- All methods go in the appropriate page files, not test files.
- Zero assertions in test files — only verification methods on page objects.
- Keep a 1:1 relationship between element/locator files and page files.

## Locator Patterns

- Prefer ID-based CSS selectors (`#id_field`) over role-based selectors when strict-mode
  violations occur.
- Use a shared helper to disambiguate ambiguous text (e.g. "Create" vs "Create & Add Another")
  rather than relying on text matching.

## Verification

- When verifying against a backend (DB, API), **retry** — flaky transaction/propagation delays are
  normal. A few attempts with a short delay handles most races.
- Unpack multi-value query results explicitly; don't index into tuples by position without naming.

## Quick Start

```bash
cd e2e_tests
pip install -r requirements.txt   # or npm install
playwright install
pytest -vs -m regression --headed  # or: npx playwright test
```
