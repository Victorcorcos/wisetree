# Reviewer Reference Tables

Curated deep-reference for the per-file review scan. The scan prompt carries
only compact name checklists; consult this file when you are unsure how to
classify or judge a suspected issue. Each table maps an issue to why it
matters and the recommended fix.

Only flag what is **actually present** in the changed code — these tables are
a checklist for recall, not a quota.

### Table A — Code Smells & Clean Code

| Smell | Reason | Solution |
|-------|--------|----------|
| **Long Method** | Hard to read, test, debug, and reuse. Violates Single Responsibility Principle and increases cognitive load. | Extract smaller, focused methods with clear names. Apply "one level of abstraction per function" rule. |
| **Step Down Rule Violation** | The code cannot be read top-to-bottom as a coherent narrative because readers must jump into unrelated helper definitions to understand the flow. | Order methods so the high-level story appears first, followed immediately by the next level of called methods. Prefer sequences like `A -> B/C`, then `B -> D/E`, then `D`, `E`, and finally `C`. |
| **God Class (Large Class)** | Centralizes unrelated logic, becomes a bottleneck for changes, difficult to test in isolation. | Apply SRP. Extract cohesive subsets of fields and methods into separate classes. Delegate responsibilities. |
| **Duplicate Code** | Every bug fix or change must be applied in all locations. Missing one introduces inconsistencies and bugs. | Extract into a shared method or class. Apply DRY. Use inheritance or composition to unify common behavior. |
| **Long Parameter List** | Hard to call correctly, easy to invoke with wrong argument order, signals method is doing too much. | Introduce a Parameter Object. Use Preserve Whole Object. Apply builder or fluent interface patterns. |
| **Primitive Obsession** | Raw primitives for domain concepts prevent validation, encapsulation, and semantic clarity. | Replace with value objects or domain types (Money, PhoneNumber). Use enums for fixed sets. |
| **Feature Envy** | A method accesses another class's data more than its own — logic lives in the wrong place. | Move the method to the class whose data it most uses. Apply Tell Don't Ask. |
| **Data Clumps** | Groups of variables always appear together — represent an unnamed concept. Error-prone to pass individually. | Extract the clump into a dedicated class or value object. |
| **Switch Statements** | Large switch/if-else chains violate Open/Closed Principle. Every new case requires modifying existing code. | Replace conditionals with polymorphism. Use Strategy or State pattern. |
| **Divergent Change** | A single class modified for many unrelated reasons — multiple responsibilities tangled together. | Separate along lines of change. Extract Class so each has one reason to change. |
| **Shotgun Surgery** | One logical change requires modifying many different classes. Makes changes fragile and expensive. | Move Method and Move Field to consolidate related behavior into a single class. |
| **Speculative Generality** | Code for future requirements that don't exist adds complexity and dead abstractions. | Delete unused abstractions. Apply YAGNI. Inline unnecessary delegation. |
| **Dead Code** | Never-executed code clutters the codebase, misleads developers, increases maintenance burden. | Delete it — version control preserves history. Use static analyzers to detect unreachable paths. |
| **Magic Numbers / Magic Strings** | Literal values carry no meaning, must be duplicated, easy to modify incorrectly. | Replace with named constants or enums with descriptive names. |
| **Inappropriate Intimacy** | Two classes constantly access each other's internals — too tightly coupled. | Move methods/fields to reduce bidirectional dependencies. Use interfaces. |
| **Message Chains** | Call chains like `a.getB().getC().getD()` expose internal structure and couple client to every intermediate type. | Apply Law of Demeter. Use Hide Delegate to encapsulate the traversal. |
| **Middle Man** | A class that only delegates to another adds indirection with no value. | Inline the class. Call the real object directly. |
| **Lazy Class** | A class that does very little introduces complexity without benefit. | Inline its contents into its caller or parent. Remove if no distinct value. |
| **Data Class** | A class with only fields, getters, setters and no behavior — anemic domain model. | Move behavior into the class. Apply Tell Don't Ask. Enrich the domain model. |
| **Refused Bequest** | Subclass inherits but doesn't use most parent methods — wrong inheritance relationship. | Replace inheritance with composition/delegation. |
| **Parallel Inheritance Hierarchies** | Every new subclass in one hierarchy requires a corresponding one in another. | Merge hierarchies. Use Strategy or Visitor patterns. |
| **Temporary Field** | Instance variable only used in certain situations, clutters the class for its entire lifetime. | Extract into a separate class or use local variables. |
| **Deep Nesting (Hadouken IFs)** | Multiple nesting levels exponentially increase cyclomatic complexity and mental load. | Apply guard clauses / early returns. Extract nested blocks. Invert conditions to flatten structure. |
| **Negative Conditionals** | Negative conditions (`!isNotValid`) increase cognitive strain and are error-prone to reason about. | Rewrite as positive conditionals. Use well-named boolean methods (`isValid`). |
| **Flag Arguments** | Boolean parameters make call sites opaque (`render(true)` — true means what?). | Split into two named methods. Replace booleans with enums or descriptive strings. |
| **Excessive Comments** | Comments compensating for unclear code. They go stale and become misleading. | Refactor to be self-explanatory: extract named methods, rename variables, simplify logic. |
| **Global Data** | Global variables / singletons readable and writable from anywhere — hidden dependencies, hard to test. | Encapsulate behind a controlled interface. Use dependency injection. |
| **Mutable Data** | Data changeable from any location introduces hidden coupling and unpredictable state. | Apply immutability. Use value objects. Isolate mutations to narrow locations. |
| **Inconsistent Naming** | Different names for the same concept force developers to learn multiple synonyms. | Establish and enforce a project-wide naming convention and ubiquitous language. |
| **Indecent Exposure** | Making internal details public allows external code to depend on internals that should be free to change. | Make everything private unless there is an explicit reason to expose it. |
| **Anemic Domain Model** | Domain objects with only data and all logic in service classes — procedural code disguised as OOP. | Move logic from services into domain objects. Apply Tell Don't Ask. |

---

### Table B — Security Issues

| Name | Risk | Solution |
|------|------|----------|
| **SQL Injection** | 5/5 | Use parameterized queries / prepared statements. Apply ORMs with strict binding. Validate and sanitize all user input. Enforce least-privilege DB accounts. |
| **Cross-Site Scripting (XSS)** | 5/5 | Encode all output contextually (HTML, JS, CSS, URL). Apply Content Security Policy (CSP) headers. Use templating engines with auto-escape. |
| **Broken Access Control** | 5/5 | Enforce server-side authorization on every request. Implement RBAC/ABAC. Deny by default. Audit access logs. |
| **OS Command Injection** | 5/5 | Avoid calling OS commands from application code. Use language-native APIs. If unavoidable, whitelist allowed commands and arguments. Never pass unsanitized input to shells. |
| **Remote Code Execution (RCE)** | 5/5 | Keep software patched. Disable dangerous features (eval, exec). Sandbox execution. Apply strict input validation. |
| **Buffer Overflow / Out-of-Bounds Write** | 5/5 | Use memory-safe languages. Apply compiler protections (stack canaries, ASLR, bounds checking). Run static analysis. |
| **Use-After-Free** | 5/5 | Use memory-safe languages or smart pointers. Apply static and dynamic analysis. Enable AddressSanitizer during testing. |
| **Cryptographic Failures** | 5/5 | Use strong modern algorithms (AES-256, RSA-2048+, TLS 1.2+). Never use MD5/SHA-1 for security. Encrypt at rest and in transit. Manage keys in vaults. |
| **Broken Authentication** | 5/5 | Enforce MFA. Use secure session management with short-lived tokens. Rate-limit and lock out after failures. Hash passwords with bcrypt/Argon2. |
| **Insecure Deserialization** | 5/5 | Avoid deserializing untrusted data. Use safe formats (JSON with schema validation). Implement integrity checks (HMAC). Run in sandboxed environments. |
| **Path Traversal** | 4/5 | Canonicalize and validate file paths server-side. Use allow-lists for directories. Never concatenate user input directly into file paths. |
| **Server-Side Request Forgery (SSRF)** | 4/5 | Validate and sanitize user-supplied URLs. Enforce allowlists for outbound destinations. Block internal IP ranges. Use network segmentation. |
| **XML External Entity (XXE)** | 4/5 | Disable external entity processing in XML parsers. Use JSON instead. Keep XML libraries patched. |
| **Security Misconfiguration** | 4/5 | Use hardened minimal configurations. Automate configuration management (IaC). Remove default credentials and unused features. |
| **Vulnerable / Outdated Components** | 4/5 | Maintain an SBOM. Monitor dependencies for CVEs with SCA tools. Automate updates. Remove unused libraries. |
| **Insecure Direct Object Reference (IDOR)** | 4/5 | Enforce authorization on every object access server-side. Use indirect references (GUIDs). Implement access control tests in CI/CD. |
| **Cross-Site Request Forgery (CSRF)** | 4/5 | Use anti-CSRF tokens. Apply SameSite cookie attributes. Verify Origin/Referer headers. |
| **Hardcoded Credentials** | 4/5 | Never embed secrets in source code. Use secrets managers. Scan repos with GitGuardian/truffleHog. Rotate credentials on discovery. |
| **Privilege Escalation** | 4/5 | Apply least privilege. Enforce strict RBAC. Audit sudo/SUID. Patch OS/kernel vulnerabilities. |
| **Improper Input Validation** | 4/5 | Validate all input server-side using strict type, length, format, and range checks. Use allowlists over denylists. Reject malformed input early. |
| **Insecure Design** | 4/5 | Integrate threat modeling early. Apply secure design patterns. Conduct security architecture reviews. |
| **Software / Data Integrity Failures** | 4/5 | Verify integrity via cryptographic signatures. Use trusted registries. Implement CI/CD pipeline security. Apply SRI for client-side assets. |
| **Supply Chain Attacks** | 4/5 | Vet dependencies carefully. Use pinning and lockfiles. Monitor for dependency confusion. Use signed packages. |
| **Sensitive Data Exposure** | 4/5 | Classify data by sensitivity. Encrypt PII at rest and in transit. Apply data masking in non-production. Enforce strict access controls. |
| **Session Management Failures** | 4/5 | Regenerate session IDs after login. Use secure, HttpOnly, SameSite cookie flags. Set short timeouts. Enforce HTTPS. |
| **Insufficient Logging / Monitoring** | 3/5 | Log authentication events, access control failures, validation errors. Centralize in SIEM. Set up real-time alerts. |
| **Integer Overflow / Underflow** | 3/5 | Use safe arithmetic libraries. Validate numeric ranges. Use compiler warnings and static analysis. |
| **Open Redirect** | 3/5 | Use allowlists of permitted redirect destinations. Resolve targets server-side using internal IDs. |
| **Clickjacking** | 3/5 | Set X-Frame-Options to DENY/SAMEORIGIN. Use frame-ancestors CSP directive. |
| **Race Condition / TOCTOU** | 3/5 | Use atomic operations and DB transactions with proper locking. Use mutexes for shared resource access. |
| **Mass Assignment** | 3/5 | Whitelist allowed fields in model binding. Use DTOs. Apply strict schema validation. |
| **Business Logic Flaws** | 3/5 | Conduct workflow abuse threat modeling. Enforce server-side business rules. Test negative/edge-case scenarios. |

---

### Table C — Performance Issues

| Name | Risk | Solution |
|------|------|----------|
| **N+1 Query Problem** | 5/5 | Use eager loading (ORM `includes`/`joinedload`). Batch queries with dataloaders. Rewrite to a single JOIN query. |
| **Missing Database Indexes** | 5/5 | Analyze slow query logs and EXPLAIN plans. Add indexes on WHERE, JOIN, ORDER BY columns. Avoid over-indexing write-heavy tables. |
| **Memory Leaks** | 5/5 | Audit for unclosed resources, lingering listeners, static collections. Use profilers (Chrome DevTools, Eclipse MAT, tracemalloc). |
| **Blocking I/O on Async Event Loops** | 5/5 | Never call synchronous blocking operations in async coroutines. Offload to thread pool (`run_in_executor`, worker threads). |
| **Cache Stampede / Thundering Herd** | 5/5 | Apply mutex so only one request regenerates cache. Use probabilistic early expiration. Stagger TTLs and implement request coalescing. |
| **Connection Pool Exhaustion** | 5/5 | Right-size pool limits. Fix leaks by ensuring connections are returned. Use PgBouncer/HikariCP for pool management. |
| **Unbounded Task Queues** | 5/5 | Replace with bounded queues. Apply backpressure. Monitor queue depth in production. |
| **Inefficient Algorithms (Poor Complexity)** | 4/5 | Profile hot paths. Replace O(n^2) with hash maps, sorting, or divide-and-conquer. Choose appropriate data structures. |
| **Long-Running Database Transactions** | 4/5 | Keep transactions short. Move non-DB work outside boundaries. Use row-level locking and statement timeouts. |
| **Excessive Lock Contention / Deadlocks** | 4/5 | Prefer fine-grained locks. Use optimistic locking for low-contention. Acquire locks in consistent global order. |
| **Thread Pool Misconfiguration** | 4/5 | Separate CPU-bound and I/O-bound pools. Size CPU pool to cores. Measure throughput and tune. |
| **Over-Indexing (Index Bloat)** | 4/5 | Audit for unused/duplicate indexes. Remove redundant ones. Periodically rebuild fragmented indexes. |
| **Chatty Microservices** | 4/5 | Aggregate in API Gateway or BFF. Batch fine-grained requests. Use async messaging to decouple services. |
| **Retry Storms** | 4/5 | Apply exponential backoff with jitter. Set max retry limits. Implement circuit breakers. |
| **Resource Leaks (Handles, Sockets)** | 4/5 | Use try-with-resources (Java), context managers (Python), `using` (C#). Run static analysis in CI. |
| **Excessive DOM Manipulation / Layout Thrashing** | 4/5 | Batch DOM reads before writes. Animate with `transform`/`opacity`. Use `DocumentFragment` for bulk inserts. Virtualize long lists. |
| **SELECT * / Over-Fetching Data** | 3/5 | Select only needed columns. Restrict API responses to required fields. Consider GraphQL for diverse field requirements. |
| **ORM Lazy Loading Misuse** | 3/5 | Identify lazy associations in loops. Switch to eager loading or explicit JOINs. Use read-model projections for complex queries. |
| **Lack of Response Caching** | 3/5 | Cache query results and API responses in Redis/Memcached. Set appropriate TTLs. Use HTTP cache headers (ETag, Cache-Control). |
| **Unoptimized Images / Static Assets** | 3/5 | Compress with modern formats (WebP, AVIF). Serve responsive sizes. Lazy load below-the-fold content. Minify and bundle CSS/JS. |
| **Excessive / Verbose Logging** | 3/5 | Restrict DEBUG/TRACE to non-production. Use async log handlers. Avoid serializing large objects in logs. Sample at high traffic. |
| **Large Payload Serialization** | 3/5 | Avoid transmitting excess data. Use binary formats (Protobuf, MessagePack) internally. Stream large payloads. |
| **Catastrophic Regex Backtracking** | 3/5 | Audit nested quantifiers (`(a+)+`). Use possessive quantifiers or atomic groups. Test against adversarial input. Enforce timeouts. |
| **Missing HTTP Compression** | 3/5 | Enable gzip or Brotli on server/gateway for text responses. Verify with response headers. |
| **Lack of Pagination** | 3/5 | Never return unbounded result sets. Implement cursor-based or offset pagination. Apply LIMIT in queries. Use virtual scrolling on client. |
| **Excessive Third-Party Scripts** | 3/5 | Audit with Lighthouse/WebPageTest. Load non-critical scripts with `async`/`defer`. Self-host critical third-party resources. |
| **Polling Instead of Push** | 2/5 | Replace with webhooks, WebSockets, or SSE for real-time updates. If unavoidable, use long-polling with exponential backoff. |
| **Serverless Cold Starts** | 2/5 | Keep packages small. Use provisioned concurrency (Lambda SnapStart, Azure always-ready). Prefer fast-startup runtimes (Python, Node.js). |
| **Hot Partitions in Distributed DBs** | 2/5 | Design partition keys with high cardinality. Add random suffix jitter. Cache hot partitions. Monitor per-partition metrics. |
| **Lack of CDN for Static Content** | 2/5 | Serve static assets through CDN edge nodes. Configure long cache TTLs with content-hash filenames. Enable HTTP/2. |

---

### Table D — Test Quality Issues

| Name | Why it matters | What good looks like |
|------|----------------|----------------------|
| **Untested Changed Lines** | A diff can look covered while the newly introduced lines are never asserted in a meaningful way. | Ensure the changed lines participate in a scenario with assertions that would fail if those lines behaved incorrectly. |
| **Missing Tests For Changed Behavior** | New or changed behavior can regress immediately with no automated signal. | Add or update tests that exercise each meaningful behavior introduced by the diff. |
| **Missing Happy Path Coverage** | The main user journey is unprotected, so the feature may not work at all despite passing CI. | Add a straightforward scenario proving the intended feature works end to end at the appropriate test level. |
| **Missing Failure / Error Path Coverage** | Error handling often breaks silently and causes user-facing failures in production. | Add tests for invalid input, dependency failures, permission failures, and expected error states relevant to the diff. |
| **Missing Branch Coverage** | Conditionals and branching logic can hide untested behavior on one side of the decision. | Cover each meaningful branch introduced or modified by the implementation. |
| **Missing Boundary / Edge Case Coverage** | Bugs usually surface at limits such as empty, nil, zero, duplicate, max-size, or out-of-range inputs. | Add tests for the common edge cases implied by the implementation and domain. |
| **Missing Regression Test** | A bug fix without a regression test is likely to reappear later. | Add a failing-then-passing test that captures the exact bug or previously broken scenario. |
| **Over-Mocked Internal Behavior** | Heavy mocking of your own classes and methods creates false confidence and fragile tests that track implementation, not behavior. | Mock only real external boundaries and use real internal flows, factories, fixtures, and collaborators whenever practical. |
| **Testing Implementation Details** | Tests become brittle and fail during harmless refactors while missing real user impact. | Assert observable outputs, state transitions, side effects, rendered UI, HTTP responses, persisted data, and domain events. |
| **Non-BDD Structure** | Poor naming and setup structure makes tests hard to read, reason about, and maintain. | Use scenario-based `describe` blocks, `before` for setup, focused `it should` outcomes, and clear nested contexts. |
| **Scattered Setup Inside Assertions** | Tests become noisy and each example reimplements the scenario differently. | Centralize setup in shared helpers or `before` blocks and keep `it` blocks focused on assertions. |
| **Assertion Weakness** | Superficial assertions let incorrect behavior slip through while still producing green tests. | Assert concrete, meaningful outcomes instead of generic truthiness or call-count trivia. |
| **Flaky / Non-Deterministic Tests** | Unstable tests erode trust in the suite and slow down delivery. | Control time, randomness, ordering, and shared state explicitly so the same inputs produce the same results. |
| **Wrong Test Level** | Too-low-level tests miss real feature behavior; too-high-level tests can be slow and vague. | Choose the smallest level that still proves the real user scenario or business behavior changed by the diff. |
| **Unclear Scenario Naming** | Test names stop acting as executable documentation. | Describe the scenario and outcome in plain language that another engineer can scan quickly. |
---
