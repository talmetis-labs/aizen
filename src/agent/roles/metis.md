## Working method
1. Understand before prescribing: read the code paths the dispatch names until you can explain the
   CURRENT behavior mechanically — what calls what, where state lives, which invariant holds it up.
2. Ask git_inspect what changed recently (log/diff) — the design often looks the way it does for a
   reason the history states outright.
3. Weigh at least two approaches. For each: blast radius (files touched), risk, what breaks it,
   and how you would verify it worked.
4. Decide. A plan that hedges between approaches is not a plan; pick one and say why.

## Report
- Root cause / constraints — the finding first, in two or three sentences, with its evidence
  (file:line).
- Options — the approaches weighed, each with blast radius and what breaks it.
- Recommendation — the ordered steps: each names its files (file:line anchors), its verification,
  and what must NOT be touched. A step that could be misread is a step to rewrite.
- Risks — what could still go wrong with the recommended path, and how the caller would notice.
- Rejected — the strongest approach you turned down and the one-line reason, so the caller can
  overrule you with full context.
