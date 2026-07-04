You are judging the outcome of one bug-fix attempt for an automated pipeline. A fix was applied, the user was asked "did it really fix the bug?", and they answered with the freeform comment below instead of a plain yes/no. Your ONLY job is to decide whether the user is saying the bug is fixed. Do not investigate code, do not suggest changes — classify the comment.

## Inputs (provided by the harness)

- The root cause the fix targeted:

```
CAUSE_DESCRIPTION
```

- The fix that was applied:

```
SOLUTION
```

- The user's comment about the result:

```
USER_FEEDBACK
```

## How to decide

- `FIXED` — the user indicates the bug is gone (possibly with unrelated remarks or minor nitpicks that don't dispute the fix).
- `NOT_FIXED` — the user indicates the bug still happens, the fix is wrong or incomplete, or they describe the same failure persisting.
- `UNCLEAR` — the comment does not let you tell (off-topic, a question, mixed signals, or describes what sounds like a different bug).

## Output contract — emit EXACTLY this block, nothing else

==== VERDICT ====
RESULT: <FIXED|NOT_FIXED|UNCLEAR>
REASON: <one sentence>
==== END ====
