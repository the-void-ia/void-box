# Markdown documentation style

Markdown documentation prose — RFCs, ADRs, files under `docs/`, and READMEs — follows the same one-paragraph-per-line rule as commit messages. Write each paragraph on a **single line**; do not hard-wrap prose to a fixed column. The renderer reflows it to the reader's display width, whereas manual mid-paragraph line breaks render as ragged text and turn a one-word edit into a multi-line diff. Reserve hard line breaks for genuinely line-oriented content (lists, tables, code blocks), and use blank lines to separate paragraphs. Apply this to prose you add or edit; reflow surrounding hard-wrapped paragraphs only when you're already changing them, not as a sweeping rewrap.

## Introduce before use; prefer mechanism over property

Design docs — RFCs especially — must read as practical descriptions of what is being built, not fluent prose that leans on terms the reader has not met. Two habits cause that defect; avoid both.

**Introduce a named component before referring to it.** The first time a concrete artifact appears (a token, a CA, a store, a config type), define it in one breath — what it is, who creates it, where it lives, its lifetime, and the problem it solves — using the indefinite article ("each run gets **a** proxy token, a random value the host…"). Only after that introduction may later text use the definite article ("the proxy token"). A first mention written as "the per-run token" presupposes a definition the reader was never given and forces them to reverse-engineer it from usage. When a doc has many such artifacts, define them once in a short "New components" inventory near the top of the design section, then refer back to it; if a forward reference is genuinely unavoidable, mark it explicitly ("the per-run CA, defined below"), never as a bare "the per-run CA".

**State the mechanism before claiming the property.** Do not assert a guarantee ("isolates runs", "bypass-safe", "fail-closed") on its own — first describe the concrete behavior that produces it, then let the property follow as a consequence. "X isolates runs from each other" is an unearned claim; "the host gives each run a token the proxy checks per connection, so one run cannot drive another's traffic" is a mechanism the isolation falls out of. The same applies to hype adjectives that assert quality without showing it ("powerful", "seamless", "robust", "simply", "easily") — describe what the thing does and let the reader judge. When reviewing prose you wrote, scan for definite-article first mentions, bare property claims, and hype adjectives, and fix each at the point of use.

## Don't narrate the document

Cut sentences whose subject is the document itself rather than its subject matter — "this section defines X first", "later sections describe each in full", "as noted above", "we now turn to", "the rest of this RFC refers to them by name". A heading already tells the reader where they are and the structure already does this work, so spelling it out only adds words to process for no information. Keep a cross-reference only when it points the reader to the specific content they need ("the per-run CA, defined below"), never as connective filler. When two phrasings carry the same information, use the shorter one.

For example, prefer "This design adds the following components:" (followed by the list) over "This design adds the following components. The rest of the RFC refers to them by name, so they are defined here first; later sections describe each in full."

## Document the design, not the deliberation

Describe what is being built and how it works — not the reasoning that produced it. Cut the internal deliberation: the alternatives weighed, why one option won, and emphatic framing that betrays a settled argument ("the whole point of X", "the same concern at two granularities", a "X, not Y" heading, a rhetorical "this raises the obvious question…"). A reader wants the mechanism and the structural constraint, not the debate that reached them. Design rationale — the alternatives and trade-offs — has a home: the RFC's "Alternatives considered" and "Risks & trade-offs" sections. State it there once; don't re-argue it in the design narrative, in an ADR, or in code comments.

For example, prefer "The proxy provides two capabilities over one pipeline: credential injection and egress control." over "Credential injection and egress are the same concern at two granularities, so one process keeps memory cost fixed rather than linear in VM count."

## Prefer standard terms; don't coin jargon

Use the field's established vocabulary rather than invented shorthand — a reader should never have to decode a term the doc made up. Avoid three habits: **coined compounds** ("daemon-vs-proxy", "name-at-the-proxy") — write what they mean ("the process boundary between the daemon and the proxy"); **metaphors standing in for a mechanism** ("the floor", "north star") — name the mechanism ("default-deny, non-bypassable network-layer enforcement"); **unexpanded abbreviations** ("per-ns") — spell them out on first use ("per-network-namespace"). When a precise term of art exists (default-deny, fail-closed, complete mediation, chokepoint), use it; otherwise describe the mechanism in plain words rather than minting a noun. If a coined term is unavoidable, define it once at first use and reuse that exact spelling — don't let it sprout variants ("network floor" / "reach floor" / "per-VM floor" for one idea).

**Don't borrow an operating context the project doesn't have.** Words like "production", "fleet", "on-call", or "SLA" presuppose a deployed system inside an organization; this project ships releases of a tool, so for it there is no "production" — only released artifacts, and the runtime path a real run takes as opposed to test fixtures and test-only paths. Name the concrete thing instead: "the backend RPC path every run uses", "the released image", "a non-test caller". One defined exception stays: the **production image** (`build_claude_rootfs.sh` output, contrasted with the test image) is an established artifact tier — reuse that exact term for that artifact and nothing else.

## Prefer unpacked prose over dense sentences

Write several short sentences rather than one that stacks many concepts. A sentence that chains three or more ideas — strung together with dashes and semicolons — forces the reader to hold all of them at once. Split it into sequential sentences, one idea each. Length is not the problem; density is. A longer passage the reader glides through beats a compact one they have to re-read. The same applies inside tables: keep a cell to its gist, and move any multi-step reasoning into a short paragraph beside the table. When you finish a paragraph, find the sentence carrying the most clauses and try breaking it in two.

## Words and sentences

The rules above shape the document; these shape its sentences and words. They split into two tiers by document type, because the same rule that sharpens an error message flattens an argument. Where a rule here meets an earlier one, the earlier one wins — in particular, target density, not length ("Prefer unpacked prose over dense sentences", above).

### Tier 1 — all prose (RFCs, ADRs, threat-model, `docs/`, READMEs)

**Prefer plain verbs and active voice.** Use the simple present, the simple past, and the imperative. Avoid the present perfect where a simple tense works ("we have identified" → "we found"). Prefer active voice; use passive only when the actor is unknown or irrelevant. Describe an action with a verb, not a noun ("compress the file", not "perform compression of the file").

**State a requirement as "must".** A requirement is "must", not "should" — a reader treats "should" as optional, and an agent reading an instruction treats it as skippable. State a recommendation as a fact ("X is better because Y"), or delete it. Use "can" for a genuine capability. Reserve "may", "might", and "could" for real uncertainty, such as a Risks section, not for hedging.

**Put a condition before its command.** In an instruction, lead with the condition, set off by a comma: "If the build fails, read the log", not "read the log if the build fails". A trailing condition is easy to miss.

**In a warning, put the command first and the risk second.** For a destructive action, state the command or the prohibition, then the consequence: "Do not run this against production. The command deletes rows." This matches the WARN and pre-flight-validation convention in AGENTS.md — name the action, then the downstream symptom.

**Delete slop; state the fact.** This extends the hype-adjective ban above with a substitution list. If a word carries no fact, delete it; otherwise replace it.

| Slop | Write instead |
|---|---|
| leverage, utilize | use |
| in order to | to |
| prior to | before |
| it is worth noting that, it's important to | (delete — state the fact) |
| simply, just, easily, seamlessly, effortlessly | (delete) |
| robust, powerful, comprehensive, performant | (delete, or give the measurable property) |
| load-bearing | (name what depends on it, or delete) |
| belt-and-braces, belt-and-suspenders | a second, redundant check (name both) |
| superseded | replaced (by); keep the RFC/ADR status label "Superseded by NNNN" |
| enables you to, allows you to | you can |
| is designed to, aims to | (delete — say what it does) |
| e.g. / i.e. / etc. | for example / that is / (name the items) |
| out of the box | by default |
| under the hood | internally |

Tier 1 sets no hard sentence length, does not ban semicolons, and does not force word swaps like "ensure" → "make sure that". A semicolon that joins two related clauses is fine; a semicolon that hides a second full idea is a signal to split. When you split a sentence, do not reintroduce document-narration ("This section defines…") — see "Don't narrate the document", above.

### Tier 2 — instructional text (AGENTS.md, CLAUDE.md, skills, error messages, WARN logs)

This is text a reader runs without asking questions, so terseness is correct. The Tier 1 rules apply, and these tighten them:

- One instruction per sentence.
- Short sentences — aim under about 20 words. A reader under stress reads each sentence once.
- No hedges, and no "should": write "must", or delete the rule.
- Always put the condition first.
- An error message follows one pattern: what happened (simple past), the cause if known, then the fix as an imperative. No "Oops", no apology filler, no "please".

### Untouchables

Never rewrite code, identifiers, CLI commands, flags, file paths, quoted errors, or config keys, even when they break a rule. A backticked command or a `file:line` reference counts as one word for the length check.

### Self-check

Before you deliver, scan the draft outside code and quoted text and fix each hit: `should` and `would`; `has been` / `have been` where a simple tense works; `, making` / `, allowing` / `, enabling`; contractions; `e.g.` / `i.e.` / `etc.`; the slop words above; and a mid-sentence `if` or `when` that belongs at the front.
