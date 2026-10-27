# Native interaction acceptance

This Draft separates implementation contracts from raw exploratory receipts.
The original research snapshot is retained outside the shipping diff; existing
Git history is unchanged. Small, bounded fixture records used by verdict tests
live under scripts/fixtures/replacement-recovery.

- The private character prototype has two completed physical appends and four
  reached negative stops: A20/B0, A221/B0, App veto and App focus handoff. These
  are experimental observations, not default-path acceptance.
- The default in-flight text path can still type into a changed focused field.
  No per-character isolation or rollback guarantee is made.
- Full-value reads, selected replacement, continuation, bounded traversal and
  native video must be reviewed and released independently with their tests.
- Text requests preserve the already-focused field: they do not run Focus
  shortcuts or unlock the phone, including short text. A locked-field error
  stops input without unlock or replay. A batch starting with text defers Focus
  for the whole batch, and dispatches a later launch only after text succeeds.
  A mock-runner regression covers both cases through the production dispatch.
- Resource-based image/video requests carry the selected instance in each URI.
  Two isolated mock daemons verify target credentials and no fallback, including
  interleaved reads and invalid targets. Host panel tests cover both default and
  named targets. Actual host application acceptance remains outstanding.

After integration with v0.25.3, 918 server library tests and 256 MCP library
tests passed (one ignored in each). The real stdio instance suite, host panel
suites, locked-text/batch-order regression and complete GitHub PR checks passed;
both iOS 17.5 / Xcode 15.4 and iOS 18.2 / Xcode 16.2 Simulator lanes passed.

The subsequent gesture merge retains #347's delivery boundaries and passes 315
Swift logic checks plus unsigned iOS compilation. The branch also includes the
v0.25.4 version metadata. These checks do not replace the remaining host,
physical and default-path acceptance lanes.
