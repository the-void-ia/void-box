---
name: rust-style
description: Rust coding style guide. Apply automatically when writing or modifying Rust code. Enforces for-loops over iterators, let-else for early returns, variable shadowing, newtypes, explicit matching, and minimal comments.
---

# Rust Coding Style

Apply these rules when writing or modifying any Rust code.

## Control Flow: Use `for` Loops, Not Iterator Chains

Write `for` loops with mutable accumulators instead of iterator combinators.

```rust
// DO
let mut results = Vec::new();
for item in items {
    if item.is_valid() {
        results.push(item.process());
    }
}

// DON'T
let results: Vec<_> = items
    .iter()
    .filter(|item| item.is_valid())
    .map(|item| item.process())
    .collect();
```

```rust
// DO
let mut total = 0;
for value in values {
    total += value.amount();
}

// DON'T
let total: i64 = values.iter().map(|v| v.amount()).sum();
```

```rust
// DO
let mut found = None;
for item in items {
    if item.matches(query) {
        found = Some(item);
        break;
    }
}

// DON'T
let found = items.iter().find(|item| item.matches(query));
```

## Early Returns: Use `let ... else`

Use `let ... else` to extract values and exit early on failure. This keeps the happy path unindented.

```rust
// DO
let Some(user) = get_user(id) else {
    return Err(Error::NotFound);
};
let Ok(session) = user.active_session() else {
    return Err(Error::NoSession);
};
// continue with user and session

// DON'T
if let Some(user) = get_user(id) {
    if let Ok(session) = user.active_session() {
        // deeply nested code
    } else {
        return Err(Error::NoSession);
    }
} else {
    return Err(Error::NotFound);
}
```

```rust
// DO
let Some(value) = maybe_value else { continue };
let Ok(parsed) = input.parse::<i32>() else { continue };

// DON'T
if let Some(value) = maybe_value {
    if let Ok(parsed) = input.parse::<i32>() {
        // ...
    }
}
```

## Pattern Matching: Minimize `if let`

Use `if let` only when the `Some`/`Ok` branch is short and there's no else branch.

```rust
// ACCEPTABLE: short action, no else
if let Some(callback) = self.on_change {
    callback();
}

// DO: use let-else when you need the value
let Some(config) = load_config() else {
    return default_config();
};

// DO: use match for multiple cases
match result {
    Ok(value) => process(value),
    Err(Error::NotFound) => use_default(),
    Err(e) => return Err(e),
}
```

## Variable Naming: Shadow, Don't Rename

Shadow variables through transformations. Avoid prefixes like `raw_`, `parsed_`, `trimmed_`.

```rust
// DO
let input = get_raw_input();
let input = input.trim();
let input = input.to_lowercase();
let input = parse(input)?;

// DON'T
let raw_input = get_raw_input();
let trimmed_input = raw_input.trim();
let lowercase_input = trimmed_input.to_lowercase();
let parsed_input = parse(lowercase_input)?;
```

```rust
// DO
let path = args.path;
let path = path.canonicalize()?;
let path = path.join("config.toml");

// DON'T
let input_path = args.path;
let canonical_path = input_path.canonicalize()?;
let config_path = canonical_path.join("config.toml");
```

## Comments: Don't Write Them

- No inline comments explaining what code does
- No section headers or dividers (`// --- Section ---`)
- No TODO comments (use issue tracker)
- No commented-out code (use version control)

Exception: Doc comments (`///`) on public items are required. See the `rustdoc` skill.

```rust
// DON'T
// Check if user is valid
if user.is_valid() {
    // Update the timestamp
    user.touch();
}

// --- Helper functions ---

// TODO: refactor this later
fn helper() { }

// Old implementation:
// fn old_way() { }

// DO
if user.is_valid() {
    user.touch();
}

fn helper() { }
```

## Type Safety: Prefer Newtypes Over Strings

Wrap strings in newtypes to add semantic meaning and prevent mixing different string types.

```rust
// DO
struct UserId(String);
struct Email(String);

fn send_email(to: Email, from: UserId) { }

// DON'T
fn send_email(to: String, from: String) { }
```

## Type Safety: Prefer Strongly-Typed Enums Over Bools

Use enums with meaningful variant names instead of `bool` parameters.

```rust
// DO
enum Visibility {
    Public,
    Private,
}

fn create_repo(name: &str, visibility: Visibility) { }

// DON'T
fn create_repo(name: &str, is_public: bool) { }
```

```rust
// DO
enum Direction {
    Forward,
    Backward,
}

fn traverse(dir: Direction) { }

// DON'T
fn traverse(forward: bool) { }
```

## Pattern Matching: Never Use Wildcard Matches

Always match all variants explicitly to get compiler errors when variants are added.

```rust
// DO
match status {
    Status::Pending => handle_pending(),
    Status::Active => handle_active(),
    Status::Completed => handle_completed(),
}

// DON'T
match status {
    Status::Pending => handle_pending(),
    _ => handle_other(),
}
```

If a wildcard seems necessary, **ask the user before using it**.

## Pattern Matching: Avoid `matches!` Macro

Use full `match` expressions instead of `matches!`. Full matches provide better compiler diagnostics when the matched type changes.

```rust
// DO
let is_ready = match state {
    State::Ready => true,
    State::Pending => false,
    State::Failed => false,
};

// DON'T
let is_ready = matches!(state, State::Ready);
```

## Destructuring: Always Use Explicit Destructuring

Destructure structs and tuples explicitly to get compiler errors when fields change.

```rust
// DO
let User { id, name, email } = user;
process(id, name, email);

// DON'T
process(user.id, user.name, user.email);
```

```rust
// DO
for Entry { key, value } in entries {
    map.insert(key, value);
}

// DON'T
for entry in entries {
    map.insert(entry.key, entry.value);
}
```

## Code Navigation: Always Use rust-analyzer LSP

When searching or navigating Rust code, always use the LSP tool with rust-analyzer operations:

- `goToDefinition` - Find where a symbol is defined
- `findReferences` - Find all references to a symbol
- `hover` - Get type info and documentation
- `documentSymbol` - Get all symbols in a file
- `goToImplementation` - Find trait implementations