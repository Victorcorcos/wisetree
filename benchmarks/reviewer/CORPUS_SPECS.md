# Public reviewer-corpus mutation specifications

These independently documented specifications define the intended behavior and minimal repair for the public evaluator-mechanics corpus. They are evaluator labels, never adapter input. The live runner's whitelist serializer removes all source, tag, ground-truth, severity, anchor, and fix metadata before a workflow runs.

| Mutation | Intended invariant | Minimal correct repair |
|---|---|---|
| `sql-interpolation` | User input never becomes SQL syntax. | Restore bound parameters. |
| `path-traversal` | Downloads stay below their configured root. | Use a containment-enforcing join. |
| `authorization-removal` | Administrative deletion requires an admin actor. | Restore the authorization guard. |
| `n-plus-one` | Listing orders performs bounded database queries. | Join or batch customer loading. |
| `unbounded-retry` | Persistent failure terminates after the retry policy. | Restore a retry bound and terminal error. |
| `blocking-async` | Async runtime workers do not perform blocking file I/O. | Use async I/O or `spawn_blocking`. |
| `duplicate-parse` | An input is parsed once per request. | Store and reuse the parsed value. |
| `partial-api-migration` | Every consumer uses the new string-backed ID. | Construct the migrated ID type consistently. |
| `deep-policy-branch` | Access policy remains a readable, auditable predicate. | Flatten conditions or name the predicate. |
| `serde-wire-name` | Rust fields preserve the documented camelCase wire format. | Restore the serde rename policy. |
| `controller-database` | Controllers delegate persistence to the repository layer. | Restore service/repository delegation. |
| `wrong-error-style` | Git subprocess errors remain recoverable and classified. | Propagate and map the error instead of panicking. |
| `missing-boundary-test` | The new maximum and above-maximum branches are protected. | Add boundary and clamping assertions. |
| `weak-error-assertion` | The new missing-token error kind is protected. | Assert the exact error variant. |
| `deleted-regression-test` | User-specific cache keys retain regression protection. | Keep an equivalent isolation test. |
| `clean-documented-constant` | A documented shared constant is allowed. | No repair; this is a false-positive trap. |
| `clean-parameterized-test` | Parameterized examples with concrete assertions are allowed. | No repair; this is a false-positive trap. |
| `clean-safe-html` | Escaped text assigned through `textContent` is safe. | No repair; this is a false-positive trap. |

Historical cases reverse real reviewed fix commits and use the forward commit and its reviewed subject as the independent minimal-repair record. Public cases exist to exercise tooling, not to support a superiority claim.
