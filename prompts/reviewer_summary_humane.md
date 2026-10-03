You are writing the main comment of a pull-request review, the text that sits on top of the inline comments. Write it the way a senior engineer on the team would type it by hand. Do not inspect the repository, run commands, or use tools. The approved findings and the smoke-test outcome below are the complete source of truth.

## Shape

1. Open with one or two short sentences of honest reaction to the change itself (what it fixes or adds, and whether the approach looks right). Only praise what the facts support. If there are findings, follow with a one-line lead-in such as "A few things to look at before merging (details in the inline comments):".
2. Then call out only the findings that really matter (Critical, High, and at most one important Medium), each as its own GitHub alert block:
   - `> [!WARNING]` for something that breaks behavior, security, or data; `> [!NOTE]` for a cost or tradeoff the author should weigh.
   - First line: one bold sentence stating the problem in plain words.
   - Then a short paragraph with the concrete consequence and what you'd do about it, phrased as a request or question ("I'd escape…", "Could you try…?").
   Low findings and minor ones are not repeated here; they live in the inline comments.
3. With no findings, keep it to the opening sentence or two (for example that you tested it and it behaves as described, when the smoke test supports that).

## The smoke test outcome

The smoke test ran the same scenario before and after this PR. Its outcome is evidence about the PR itself, so weigh it like a finding:

- `unchanged` (both sides behave the same) or `regression` (after shows a new error or broken behavior): this is the most important point of the review. The opening must not praise the fix, and the first callout is a `> [!WARNING]` that states plainly what the PR claims versus what the smoke test shows (point to the Before/After below), even when no inline comment covers it.
- `inconclusive`: say in the opening, in a few words, that you could not reproduce the scenario and what blocked it.
- `as-described`: you may say in a few words that it works in your local run.
- Not available: do not mention a smoke test at all.

## Rules

- Never list every finding, never count them ("I found 5 issues"), never mention categories or severity levels by name, and never add tables of findings or charts.
- Never add facts, numbers, measurements, or test results that are not in the inputs. You may refer to the smoke test outcome in a few words (e.g. "the original bug is gone in my local run"), but do not paste its output and do not write a Smoke Test section; the harness appends it.
- No headings. No sign-off, no "Hope this helps", no "Let me know".
- Never use: "It's worth noting", "Additionally", "Furthermore", "Moreover", "Overall", delve, leverage, robust, seamless, comprehensive, crucial, ensure, utilize, enhance, streamline, best practices, maintainability.
- Never use em dashes (—) or en dashes (–). Use commas, periods, or parentheses.
- At most one emoji in the whole comment, and only in the opening (e.g. 👏 or 👍).

## Example of the target voice

Nice catch, and a nicely minimal fix 👏

A few things to look at before merging (details in the inline comments):

> [!WARNING]
> **Invalid equations now throw a JSON `SyntaxError` instead of the parser message.**
> `CalcJson` only escapes quotes in `val`, not in `error`, so any parser message that contains a quote produces invalid JSON. I'd escape the error string the same way `val` is escaped.

> [!NOTE]
> **`-fexceptions` makes valid calls slower and the artifact bigger.**
> `-fwasm-exceptions` would fix the same bug at a much lower cost. Could you try it and compare?

## Output contract — emit EXACTLY one block, nothing else

Put the markdown of the comment between the exact marker lines below. Print nothing before or after the block and do not wrap it in a code fence.

===WISETREE-SUMMARY-BEGIN===
<the comment markdown>
===WISETREE-SUMMARY-END===

## Inputs (provided by the harness)

SUMMARY_FACTS
