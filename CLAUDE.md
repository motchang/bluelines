# CLAUDE.md

## Merging pull requests

This repo is also reviewed by Codex (`chatgpt-codex-connector[bot]`). Never merge a
pull request until Codex has finished reviewing its current head commit:

- Codex keeps a summary comment on the PR (`<!-- codex-pull-request-review-summary -->`).
  Its `codex-review-state:v1` JSON must show `code_review.status` = `completed`,
  `code_review.commit_sha` matching the PR's current head, and
  `has_unresolved_findings` = `false`.
- Codex does not re-review on push. If the head has moved since its last review,
  comment `@codex review` on the PR and wait for the new result.
- Codex never approves. A 👍 reaction from it on the PR means it found nothing.
- Address or reply to every Codex finding before merging.
