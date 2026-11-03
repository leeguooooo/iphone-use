# Delayed loaders and retry receipts

A negative wait can pass before the target ever appears: a tap returns, its
loader appears later, and an immediate “loader absent” read reports success.
Use `absent_after_present:true` to require each target to appear in this wait
before disappearance can complete it. A never-seen loader times out. This is
opt-in because a valid task may have no loader at all.

The separate failure is replaying a batch because its last step was not sent.
Earlier actions may already have applied. Keep failed-step and whole-batch
receipts distinct; expose a retry strategy only with positive not-delivered
evidence for the whole request.

The native synthetic checks in `scripts/test-native-element-evidence.py` cover
bounded subtree calls and action gates. `disappearance_tests` covers per-target
appearance history. `action_feedback` tests cover uncertainty, partial batches,
secure values and business-outcome limits. `action_trace` and router tests cover
opt-in, privacy redaction and owner isolation.
