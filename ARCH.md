# ARCH.md — Construction blueprint

> **For builder agents.** Short by design: negatives (forbidden edges/patterns)
> don't rot; positive specs do. Inject into agent context at session start. A
> change that breaks one of these is DRIFT, not a fix — stop and update this
> file (with a reason + beads issue) first.

## What this is (2-line positive anchor)

<!-- TODO: one or two lines on what this system IS. e.g. "A daemon that owns X
     over LAN, turns events into Y, and does Z — no cloud in the runtime path." -->

## Negative invariants (forbidden — breaking one is drift, not a fix)

<!-- TODO: 5-10 lines. Phrase each as a NEGATIVE ("X must not...") and make it
     machine-checkable where possible (a forbidden edge, a single-ownership rule,
     a layering constraint). Examples:
       1. <core module> depends on NO other internal module (innermost ring).
       2. Nothing depends on the <daemon/binary> crate (it's a leaf).
       3. <cheap thing> runs BEFORE <expensive thing> (never call the expensive one raw).
       4. Only <one owner> may touch <shared resource> (single-writer rule).
     Negatives stay true for years; that's why this file is negatives, not a
     full architecture description. -->

## When this file is wrong

If a task genuinely requires breaking an invariant, update THIS file first (with
a beads issue + reason), then change the code. A silent violation is the exact
late-caught drift this file exists to prevent.
