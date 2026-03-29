---
name: rust-analyzer-ssr
description: Use rust-analyzer's Structural Search and Replace (SSR) to change lots of Rust code. SSR matches by AST structure and semantic meaning, understanding type resolution and path equivalence.
---

# rust-analyzer Structural Search and Replace (SSR)

Use rust-analyzer's SSR for semantic code transformations in Rust projects. SSR matches code by AST structure and semantic meaning, not text.

## When to Use

- Refactoring patterns across a codebase (rename, restructure, migrate APIs)
- Converting between equivalent forms (UFCS to method calls, struct literals to constructors)
- Finding all usages of a specific code pattern
- Semantic-aware search that understands type resolution

## Basic Syntax

```
<search_pattern> ==>> <replacement_pattern>
```

Placeholders capture matched code:
- `$name` — matches any expression/type/pattern in that position
- `${name:constraint}` — matches with constraints

## Common Patterns

### Swap function arguments
```
foo($a, $b) ==>> foo($b, $a)
```

### Convert struct literal to constructor
```
Foo { a: $a, b: $b } ==>> Foo::new($a, $b)
```

### UFCS to method call
```
Foo::method($receiver, $arg) ==>> $receiver.method($arg)
```

### Method to UFCS
```
$receiver.method($arg) ==>> Foo::method($receiver, $arg)
```

### Wrap in Result
```
Option<$t> ==>> Result<$t, Error>
```

### Unwrap to expect
```
$e.unwrap() ==>> $e.expect("TODO")
```

### Match only literals
```
Some(${a:kind(literal)}) ==>> ...
```

### Match non-literals
```
Some(${a:not(kind(literal))}) ==>> ...
```

## Constraints

| Constraint | Matches |
|------------|---------|
| `kind(literal)` | Literal values: `42`, `"foo"`, `true` |
| `not(...)` | Negates inner constraint |

## How to Invoke

### Via Comment Assist (Interactive)
Write a comment containing an SSR rule, then trigger code actions:
```rust
// foo($a, $b) ==>> bar($b, $a)
```
Actions appear: "Apply SSR in file" or "Apply SSR in workspace"

### Via LSP Command
```json
{
  "command": "rust-analyzer.ssr",
  "arguments": [{
    "query": "foo($a) ==>> bar($a)",
    "parseOnly": false
  }]
}
```

### Via CLI
```bash
rust-analyzer ssr 'foo($a, $b) ==>> bar($b, $a)'
```

## Key Behaviors

**Path Resolution**: Paths match semantically. `foo::Bar` matches `Bar` if imported from `foo`.

**Auto-qualification**: Replacement paths are qualified appropriately for each insertion site.

**Parenthesization**: Automatic parens added when needed (e.g., `$a + $b` becoming `($a + $b).method()`).

**Comment Preservation**: Comments within matched ranges are preserved.

## Macro Handling

SSR can match code inside macro expansions, but with an important restriction: **all matched tokens must originate from the same source**.

### Example: Macro Boundary

```rust
macro_rules! my_macro {
    ($x:expr) => {
        foo($x, 42)  // "42" comes from macro definition
    };
}

my_macro!(bar);  // "bar" comes from call site
```

The expanded code is `foo(bar, 42)`. Here:
- `bar` originates from the **call site** (what the user wrote)
- `foo`, `42` originate from the **macro definition**

If you search for `foo($a, $b)`:
- It would **NOT** match the expanded `foo(bar, 42)` because `$a` would capture `bar` (call site) but `$b` would capture `42` (definition site) — these cross the macro boundary.

### Why This Limitation Exists

SSR can only generate edits for code the user actually wrote. If a match spans both user code and macro-generated code, SSR couldn't produce a valid edit — it would need to modify the macro definition, which is a different (and potentially shared) piece of code.

### What SSR CAN Do With Macros

- Match code entirely within macro arguments: `my_macro!(foo($a))` can match `foo($a)`
- Match the macro call itself: `my_macro!($x)` works
- Match expanded code where all tokens come from call-site arguments

## Other Limitations

- Constraints limited to `kind(literal)` and `not()`
- Single-identifier patterns (`foo ==>> bar`) may be filtered if ambiguous
- Cannot modify `use` declarations with braces

## More Examples

### Convert Option methods
```
$o.map_or(None, Some) ==>> $o
```

### Change field access
```
$s.foo ==>> $s.bar
```

### Reorder struct fields
```
Foo { a: $a, b: $b } ==>> Foo { b: $b, a: $a }
```

### Generic type transformation
```
Vec<$t> ==>> SmallVec<[$t; 4]>
```

## Source Files

- Core implementation: `crates/ide-ssr/src/`
- IDE integration: `crates/ide/src/ssr.rs`
- Tests with examples: `crates/ide-ssr/src/tests.rs`