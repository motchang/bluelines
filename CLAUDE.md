# CLAUDE.md

## プルリクエストのマージ

このリポジトリは Codex (`chatgpt-codex-connector[bot]`) によるレビューも受けています。
Codex が現在の head コミットのレビューを終えるまで、プルリクエストを絶対にマージしないでください。

- Codex は PR 上にサマリーコメント (`<!-- codex-pull-request-review-summary -->`) を保持しています。
  その `codex-review-state:v1` JSON が、`code_review.status` = `completed`、
  `code_review.commit_sha` が PR の現在の head と一致、
  `has_unresolved_findings` = `false` であることを確認してください。
- Codex は push されても再レビューしません。前回のレビュー以降に head が動いている場合は、
  PR に `@codex review` とコメントし、新しい結果を待ってください。
- Codex は approve しません。PR に Codex から 👍 リアクションが付いていれば、指摘事項なしという意味です。
- マージ前に、Codex のすべての指摘に対応するか返信してください。
