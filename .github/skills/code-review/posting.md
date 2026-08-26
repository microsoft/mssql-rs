# Posting a Review

Mechanics for submitting a review to GitHub. Referenced from `SKILL.md`; read this
only once posting has been authorized.

- `gh pr review --comment --body-file` posts **only** a top-level body. For inline
  comments use the API directly:

  ```bash
  gh api repos/microsoft/mssql-rs/pulls/<N>/reviews -X POST --input review.json
  ```

  ```jsonc
  {
    "commit_id": "<head sha>",
    "body": "## Summary ...",
    "event": "COMMENT",
    "comments": [
      { "path": "mssql-tds/src/foo.rs", "line": 634, "side": "RIGHT", "body": "..." }
    ]
  }
  ```

- Inline comments must land on lines present in the diff or the API returns 422.
  A hunk header `@@ -115,21 +143,206 @@` means that hunk covers new-side lines
  143-348; a line outside it is only anchorable if some *other* hunk covers it. Check
  the full set with `git diff $BASE..HEAD -- <file> | grep -n '^@@'` rather than
  reasoning from the nearest one. Findings on lines no hunk covers belong in the
  top-level body.
- On your own PR, `COMMENT` reviews with inline comments are allowed — only `APPROVE`
  and `REQUEST_CHANGES` are blocked. Don't downgrade to a single top-level comment.
- Verify what landed; the comments endpoint pages at 30, so a fresh review looks
  missing without `--paginate`:

  ```bash
  gh api --paginate repos/microsoft/mssql-rs/pulls/<N>/comments \
    -q '.[] | select(.pull_request_review_id==<ID>) | "\(.path):\(.line)"'
  ```

- Remove mid-sentence line wrapping from the body before posting so GitHub can wrap
  it. Wrapping is fine while previewing in chat.
