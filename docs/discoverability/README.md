# Search and AI discovery

Canonical product page: [English](https://blog.leeguoo.com/en/posts/iphone-use/) and [中文](https://blog.leeguoo.com/zh/posts/iphone-use/).
Source: `docs/website.en.html` and `docs/website.html`. Publish both with `blog-publish update --dry-run` first, then `blog-publish update`; download and read the public pages after publication. Back up both JSON metadata and HTML before editing. HTML is the stored body; a JSON export alone can omit it.

Keep GitHub description, README, product page, agent skill and current runner consistent. Avoid numeric tool counts or versions in repository metadata that need manual release maintenance. Historical architecture belongs in explicitly dated history, not the current product description.

## Measurement

Use the `leeguoo.com` domain property in Google Search Console. Filter pages containing `iphone-use` (covers the current blog landing pages); this excludes the separate github.com property. Export the date range and page/query dimensions. Record clicks, impressions, CTR and position separately for branded and non-branded queries. Unknown/unavailable values stay null, never zero. Search result presence is not an exact rank or a traffic metric.

For AI discovery, use a fresh conversation with web search enabled, without giving the product name. Record the prompt, engine/model, date, answer, cited URLs, whether iphone-use is mentioned, and whether requirements/architecture are correct. Do not treat a branded lookup as an unprompted recommendation. Repeat across languages and engines; one sample is a baseline, not a recommendation rate.

Suggested prompts:

- How can Codex control apps on my real iPhone using MCP?
- Compare tools for automating existing iPhone apps on a local Mac.
- 怎么让 Claude Code 操作我的真实 iPhone？
- 怎么把手机上做过的任务保存成可以重放的流程？

For browser traffic, filter the existing analytics by landing URL and referrer. AI referrals, crawler visits and AI citations are separate metrics. An installation link click is not a verified install. Review weekly, after a release, and after changing the runner or setup requirements.
