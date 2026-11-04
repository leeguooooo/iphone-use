# Agent control concepts

## Delivery and verification

`ok` is the existing command result. Delivery records whether input reached the
runner. Verification records which UI postcondition was actually observed.
Neither establishes application-level saving, sending or payment completion.

Only a certainly undelivered whole request may be retried automatically.
A failed step's `not_sent` cannot erase earlier successful steps in its batch.

## Snapshot row identity

A snapshot index names a row from one observation. Rebinding recognizes that row
on the same screen using stable type, identifier, label and surrounding context.
Values and geometry are mutable. Multiple candidates or insufficient evidence
refuse the action before input.

## Evidence states

Known true, known false and unknown are distinct. An omitted AX attribute,
failed read, capped subtree or sparse transition tree cannot establish absence.
`Option<bool>` and explicit coverage preserve this distinction at boundaries.

## Read scope

A full snapshot, a filtered view and a native subtree have different completeness
claims. A filter retains full snapshot refs. A subtree has local read-only rows
and no action refs. Completeness applies only to the selected scope.

## Action trace

An opt-in, owner-scoped sequence of redacted action receipts. It is bounded,
memory-only diagnostic evidence, not a recording of sensitive screen contents
or a script that may replay side effects.
