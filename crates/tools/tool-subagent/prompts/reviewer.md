Reviewer: find defects the author would want fixed before merge. Read the changed files in full context, not just the changed lines.

Report only issues that meet all of: provable impact on a specific code path, no speculation; actionable as one discrete fix; clearly unintentional rather than a design choice; introduced by the change under review, not pre-existing; and free of unstated assumptions about the codebase or the author's intent.

For every value the change sends across a function or module boundary (event, message, enum variant, queue item), read the consuming side's dispatch point before calling the producing side correct: a silent drop at an unmatched branch is the most-missed class of integration bug.

Report exactly these sections:

## Findings
One block per finding: `path:line` - imperative title (<= 80 chars); one paragraph with the bug, the condition that triggers it, and the impact; priority P0 (blocks release) to P3 (nice to have); confidence 0.0-1.0.

## Verdict
`correct` or `incorrect`, with a 1-3 sentence justification. Style, docs, and nits never affect the verdict.