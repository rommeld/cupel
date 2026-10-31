# Rust Coding Guide

Source: [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html).
This document summarizes that page only; linked documents have not been incorporated.
Platform-specific infrastructure rules are omitted. Project rules retained from the former
`AGENTS.md` coding guidelines are explicitly labeled.

Panics are permitted only in tests, not in application code. Source recommendations that
permit application-code panics are omitted.

Examples are excerpts from the source. Some are schematic: `...` denotes omitted code.
Code labeled **Avoid** illustrates a pattern not to follow.

## Goals and scope

- Prioritize readability and maintainability over ease of writing: code is expected to
  remain in use for a long time.
- Favor consistency and patterns that support automated, codebase-wide changes.
- Stay consistent with the wider Rust ecosystem where possible.
- Prefer rules with tangible benefits; minimize rules that only settle minor disagreements.
- Treat API-design rules as binding for libraries intended for other projects.
- For application code, treat API-design rules as strong recommendations. Prefer simplicity
  and directness over broad reusability; avoid premature generalization and abstraction.

### Relationship to public Rust guidance

**Rationale:** Consistency, readability, and established API-design choices reduce repeated
style debates. Deliberate, refactoring-friendly patterns also support automated large-scale
changes.

Follow the public Rust Style Guide and Rust API Guidelines where they do not conflict with
this guide. Most formatting conventions are enforced by `rustfmt`.

## Code organization

A crate is a compilation unit containing a root module. Modules organize names and
visibility within a crate and can split code across files.

### Prefer small crates

**Rationale:** Fine-grained library targets help limit the effects of dependencies in large
build graphs and improve parallel compilation and build caching.

- Prefer small crates focused on one abstraction, ideally, but not necessarily, one file.
- First decide whether APIs need to share a crate; only then consider submodules.
- The root module may re-export public APIs from leaf modules to hide internal organization.
- Larger crates are acceptable when trait-implementation coherence requires related types
  and traits to share a library, or for large foundational libraries.

### Name files and leaf modules after their primary item

**Rationale:** Matching names make a file's or module's content and purpose apparent.

If a leaf module primarily defines `Foo`, name it `foo` and its file `foo.rs`. Modules not
centered on one abstraction may use another appropriate name.

### Keep implementation in leaf modules

**Rationale:** Separating structure from contents makes implementations easier to find.

Non-leaf modules should contain module declarations, not inline module definitions, and
re-exports. A small amount of code tying core pieces together is acceptable. Test modules
and cross-language binding modules do not prevent a module from counting as a leaf.

### Avoid glob imports and preludes

**Rationale:** Glob imports complicate automated changes and obscure where types originate
when cross-reference tooling is unavailable.

Do not use glob imports, except optionally for:

- `use super::*` in inline modules.
- `use enum::*` in enum method bodies or functions primarily handling that enum.
- `pub use submodule::*` in non-leaf modules.
- Merging C++ namespaces from different Rust crates into a private module. The resulting
  merged module must not be public.

For example, the namespace-merging exception permits:

```rust
mod absl {
    pub use status::absl::*;
    pub use strings::absl::*;
}
```

A reviewer may allow a third-party crate's recommended prelude. Avoid defining preludes.

### Import items from one module per `use` statement

Do not put nested `::` paths inside a grouped import. This corresponds to
`imports_granularity = Module` in `rustfmt`.

**Prefer:**

```rust
use foo::Foo;
use foo::bar::{Bar, Baz};
```

**Avoid:**

```rust
use foo::{Foo, bar::Bar};
```

### Scope trait imports locally

Import a trait inside a function unless its methods are used extensively throughout the
file.

```rust
fn foo(s: &str) -> Result<u8, std::num::ParseIntError> {
    use std::str::FromStr;

    u8::from_str(s)
}
```

### Qualify top-level function calls with their module

**Rationale:** Consistent qualification lets readers assume an unqualified lowercase
function name is local, even without cross-reference tooling.

Import the module and call `modulename::functionname()` for functions from another module
or crate. Exceptions include sufficiently unambiguous names, functions from small,
single-purpose first-party crates, and module names prone to collisions, such as C++
namespaces repeated across crates.

### Import type and trait names for unqualified use

**Rationale:** A consistent convention avoids repeated style disagreements.

Prefer `use regex::Regex` followed by `Regex`, not repeated `regex::Regex` paths. Exceptions:

- Ambiguous common names: use `io::Error` or an alias such as `IoError`.
- Large groups of related types that are clearer under a module qualifier.
- Macros and generated code where requiring imports is counterproductive.
- Dependency-heavy code where importing everything creates a large, unhelpful import block.

**Project rule:** Prefer `crate::` over `super::` for paths from the crate root.

## Formatting

Use the project's formatting tool. Formatting preferences listed in the source are:

- A maximum width of 100 columns: `max_width = 100`.
- Lowercase hexadecimal digits: `hex_literal_case = Lower`, such as `0xab123cdef`.
- Macro matcher formatting: `format_macro_matchers = true`.
- Escapes for invisible characters in literals, such as `\u{200D}` for a zero-width joiner.

A codebase targeting stable Rust may omit formatting options unavailable on stable.

Use `#[rustfmt::skip]` only for situation-specific manual alignment, such as matrices or
similar lines aligned for readability, or for generated code. Prefer generating such code
through the build system. Do not skip formatting for personal style preferences.

## Ownership and borrowing

### Prefer unique ownership over `Rc` and `Arc`

**Rationale:** Shared-ownership pointers add runtime overhead and their own complexity.
Rust can often represent object relationships without them.

Prefer unique ownership and borrowing. Use `Rc` or `Arc` when the alternative would be
unreasonably difficult to write, understand, or maintain; discuss that trade-off in review.

**Project rule:** Avoid global state such as `lazy_static!` or `OnceCell`-based shared state.
Pass explicit context to keep dependencies visible and tests straightforward.

### Borrow instead of making unnecessary deep copies

**Rationale:** Deep copies can require memory allocations and data copying.

Use `.clone()` when a deep copy is genuinely needed, not as a substitute for designing
borrowing relationships. Cloning can be pragmatic when those relationships become
excessively complex. Consider the code's performance needs.

Cloning `Rc` or `Arc` does not make a deep copy and is an expected way to use those types.

**Project rule:** When pairing results with their source data, borrow it through explicit
lifetimes rather than cloning it, for example a `ValidationResult` containing `&Commit`.

## Function inputs, outputs, and errors

### Accept flexible references to immutable data

**Rationale:** References independent of particular containers let callers supply data
stored in different ways without copying it first.

| Accept | Instead of |
| --- | --- |
| `&T` | `&Box<T>` |
| `&[T]` | `&Vec<T>` |
| `&str` | `&String` |
| `Option<&T>` | `&Option<T>` |

Non-linear character storage, such as ropes or cords, is an exception.

### Prefer iterator return types where useful

**Rationale:** Returning an iterator lets callers collect, loop, or chain adapters and can
avoid allocations. In application code, known usage patterns may make a vector simpler.

Prefer a concrete iterator or `impl Iterator` over `Vec<T>` where possible. Returning a
vector is appropriate when:

- Most callers need an owned vector.
- The implementation naturally constructs one or returns an existing one.
- Callers likely need random indexing.
- The algorithm is clearer as a `for` loop and allocation is not important.
- Creating a new owned collection is the function's purpose.

In application code, a vector is fine when it simplifies the code and iterator flexibility
or performance is not clearly needed.

### Accept iterators when only iteration is required

**Rationale:** Iterators let reusable libraries accept data from different collections,
avoid copies into contiguous storage, and compose with iterator chains. Application code
often has more predictable callers and may benefit more from a simple slice parameter.

Highly reusable libraries should accept iterators or `impl IntoIterator` when they only
need to iterate. Application code may accept `&[T]` when this is simpler and iterator
flexibility or performance is not clearly needed.

### Use `impl Into<T>` selectively

**Rationale:** `impl Into<String>` accepts both `String` and `&str` directly, but generic
parameters complicate signatures, increase code size, and sometimes hinder type inference.

Consider `impl Into<T>` in library APIs when callers have different compatible types and
call-site ergonomics are paramount. Do not use it indiscriminately. Application code
should almost never use `impl Into<T>` parameters.

### Prefer descriptive enum parameters over unclear booleans

Use an enum when a boolean parameter's purpose is not obvious at the call site. If using a
boolean, consider an inline argument comment explaining its meaning.

### Report failures with `Option` or `Result`

- Use `Option` for simple, anticipated failures whose cause is obvious and needs no
  additional detail, such as a missing value or parsing mismatch.
- Use `Result` for recoverable failures where callers need context to distinguish failure
  modes and respond appropriately, particularly external failures such as I/O or networking.
- Under this project's no-application-panics policy, report broken invariants through
  `Result` as well.

```rust
// The failure cause is obvious: integer overflow.
fn checked_add(a: u32, b: u32) -> Option<u32> { ... }

// The caller needs to distinguish NotFound from PermissionDenied.
fn open_file(path: &Path) -> Result<File, io::Error> { ... }
```

**Project rules:**

- Categorize errors by what callers can do: retry, skip, or fail.
- Add meaningful context at module boundaries rather than blindly forwarding errors.
  Propagate with `?`; add context with `.context()` or `.with_context()`.
- For recoverable cases, use appropriate combinators such as `.unwrap_or_default()`,
  `.unwrap_or_else(...)`, or `.ok_or_else(...)`. Reserve `.unwrap()` for tests.
- Validation checks should return `Option<Finding>` rather than a boolean when returning a
  finding is appropriate: `if let Some(f) = check(...) { findings.push(f) }`.

## Types

### Represent valid states explicitly

**Project rule:** Use enums for mutually exclusive states. Prefer typestate when it can
make invalid states unrepresentable by exposing methods only on valid state types.

### Choose newtypes when identity or semantics matter

Use a newtype instead of a type alias when:

- The type has notably different semantics or needs its own API.
- The underlying representation may need to change, especially when usages are outside
  your control.
- Distinct types prevent accidental mix-ups, such as `Day`, `Month`, and `Year` in date
  calculations.

### Prefer named fields

Prefer named fields over tuples and multi-field tuple structs; indexes such as `.0` and
`.1` quickly become confusing. Single-field newtypes and very local, unambiguous tuple
usage are exceptions.

## Traits

### Add implementations when appropriate

- Low-level libraries should proactively anticipate users' trait needs.
- Application code should add trait implementations as they become necessary.
- Implement a matching standard library trait instead of a custom equivalent method.
  For example, implement `FromStr for Foo` instead of only adding an inherent
  `fn from_str(&str) -> Self`. Providing both is acceptable for discoverability.
- Prefer `#[derive(...)]` over an equivalent manual implementation.

**Project rule:** For enums parsed from strings, implement `FromStr` with descriptive
errors listing valid options. Support useful aliases, such as `ref` for `MissingReference`.

### Prefer generics to `dyn Trait` in libraries

**Rationale:** Dynamic dispatch requires indirection and vtable lookups and prevents some
optimizations, such as inlining. Some traits are not compatible with `dyn` at all, including
traits with generic functions, associated functions without `self`, or functions returning
`Self`.

Prefer `T: Trait` or `impl Trait` parameters in libraries. Code-size constraints are an
exception.

**Project rule:** Use associated types when there is one natural implementation per type;
use generic parameters when multiple implementations make sense. Keep traits compatible
with `dyn Trait` when dynamic dispatch is needed.

### Define traits for abstraction, not organization

Define a trait only to:

- Enable generic programming through trait bounds.
- Accept or return `impl Trait` or `dyn Trait`.
- Provide an extension trait, subject to the rules below.

Otherwise, use inherent implementations. Do not introduce traits merely to organize code.

### Keep extension traits with the original type's owner

**Rationale:** Having the same team own a type and its extension trait helps avoid
method-name conflicts with inherent methods or other extension traits.

An extension trait adds methods to a type from another crate; its purpose is not generic
programming or dynamic dispatch. Name it with an `Ext` suffix. In general, the team owning
the original type should also own the extension trait. Well-known, substantially valuable
extensions in foundational libraries or frameworks are an exception.

```rust
extern crate foreign_crate;
use foreign_crate::ForeignType;

trait ForeignTypeExt {
    fn additional_method(&self);
}

impl ForeignTypeExt for ForeignType {
    fn additional_method(&self) { ... }
}
```

An extension trait can also have a blanket implementation for every type implementing a
foreign trait, effectively adding default methods to that trait. Traits intended as generic
inputs and implemented on foreign types are not extension traits merely for that reason.

### Choose standard traits deliberately

- **Public-field types:** Consider traits implemented by every field.
- **Error types:** Implement `Copy` when possible; passing by value then avoids ownership
  moves. Errors containing non-`Copy` data such as `String` cannot do this.
- **`Copy`:** Implement when it is reasonably clear the type can remain `Copy` in the future.
- **`Clone`:** Most types should implement it unless they represent external resources.
- **`Default`:** Implement for types with `new()` or another default state.
- **`PartialEq` / `Eq`:** Prefer `PartialEq` when enabling `==`; `Eq` additionally requires
  reflexivity.
- **`PartialOrd` / `Ord`:** Prefer `PartialOrd` when enabling `<` or `>`; `Ord` requires a
  total ordering.
- **Map keys or cached types:** Implement `Hash` and `Eq`; also implement `Ord` and its
  required traits if there is a clear total ordering.

### Make important conversions discoverable

When a `From` implementation is a primary construction path, also provide an inherent
`new()`, `new_from_foo()`, or `from_foo()` constructor.

### Implement `Default` for options structs

**Rationale:** `Default` supports convenient struct update syntax:
`Struct { a: 0, ..Default::default() }`.

Implement `Default` for all-public-field structs primarily used as a bag of function
options.

**Project rule:** Layer configuration in this order: defaults, file configuration through
`apply_file_config()`, then CLI overrides through `apply_cli_overrides()`. Each layer
mutates the same configuration object.

## Concurrency

Never access the protected contents of a poisoned mutex.

## Macros and code generation

**Rationale:** Macros make code harder to understand and write correctly, can create local
language dialects, weaken developer tooling, and complicate large-scale changes.
`macro_rules!` operates on syntax trees and can be less fragile than textual generation
for simple code generation.

Prefer these mechanisms in order:

1. Ordinary Rust functions, types, traits, objects, and modules.
2. `macro_rules!` macros.
3. Other code generation strategies, such as build-system generation rules.
4. Procedural macros.

Weigh the benefit of macros or generation against their readability, tooling, and
maintenance costs.

## Naming

- **Methods:** Follow standard library conventions, including appropriate receiver
  conventions for `as_...()`, `to_...()`, and `into_...()`.
- **Lifetimes:** Prefer descriptive names, especially on types. Anonymous `'_` is fine.
  Extremely generic abstractions such as `Cow` may use short names. Up to two local
  single-character lifetimes are fine for input/output relationships or higher-ranked
  bounds such as `for<'a>`; use descriptive names when more are needed.
- **Generic parameters:** Use short names related to their purpose, preferably reflecting
  the most important trait bound. `T` and `U` are fine for broadly applicable types such
  as `Vec<T>`.
- **Extension traits:** Use an `Ext` suffix, such as `ForeignTypeExt`.
- **Constructors:** Use `new()` or `new_with_foo()` for alternatives; use `try_new()` when
  returning `Result`.
- **Value conversions:** Use `from_foo()` with a corresponding `From` implementation.
- **Abbreviations:** Avoid unfamiliar project-specific abbreviations and removing letters
  from words. Common abbreviations, including ones found in Wikipedia, and clearly
  understandable names such as `IpAddr` are exceptions. Prefer spelling words out.

## Documentation

**Rationale:** Rust documentation comments are available to `rustdoc`, IDEs, and other
tools; Markdown gives them a standard rich-text structure. Their syntax is checked, and
their presence in the syntax tree supports tooling and macros.

- Document public APIs; consider the `missing_docs` lint.
- Include enough examples somewhere in the documentation to demonstrate the crate's
  functionality; not every function needs its own example.
- Clearly document invariants of unsafe functions and traits.
- Use capitalization and punctuation. Explain useful information, not language basics or
  what the code already says.
- Omit trivial, repetitive documentation rather than repeating an item's name; allowing
  the lint is acceptable in that case. Self-explanatory enum variants may go undocumented.
- Use `///` for the following item and `//!` at the top of a file for its module or library.
- Use Markdown. Use ordinary `//` comments for implementation details and internal TODOs.
- Prefer directly readable documentation comments over macro-generated documentation.
- Combine documentation and implementation comments when they serve different audiences.

Source example, with the project-specific issue reference removed:

```rust
/// Finds the first `FooItem` with the given `name`, or `None` if not present.
// TODO: `name` should be &str, not String.
pub fn get_named(items: &[FooItem], name: String) -> Option<&FooItem> { ... }
```

**Avoid** comments that merely repeat the name:

```rust
/// Removes an element.
pub fn remove_element() -> Element {}
```

## Other Rust features and API contracts

### Use safe code whenever possible

Do not write unsafe code if equivalent functionality can be implemented safely. When
unsafe code is necessary:

- Encapsulate it in a safe abstraction, separated from business logic. Ideally, isolate it
  in a crate with few dependencies beyond the standard library.
- Add a `SAFETY:` comment explaining correctness.
- Provide a technical justification for the block, file, or build target: why is an
  equivalent safe implementation not used? If the reason is performance, are there
  benchmarks?

### Avoid unstable features

Do not use unstable features. Exceptions require review: embedded code may need features
essential to building an artifact correctly; project maintainers may approve a small
codebase-wide list, typically for features with a clear path to stabilization.

### Use consistent accessor names

| Name | Behavior |
| --- | --- |
| `foo()` | Return `&T`, or `T` when `T: Copy`. |
| `foo_mut()` | Return `&mut T`. |
| `take_foo()` | Reset the property to `None` or another default and return the old value. |
| `set_foo()` | Accept `T` by value and set the property. |
| `with_foo()` / `with_mut_foo()` | Access the property through a closure. |
| `replace_foo()` | Set the property and return the old value. |

```rust
impl Stuff {
    fn foo(&self) -> &Foo { ... }
    fn foo_mut(&mut self) -> &mut Foo { ... }
    fn take_baz(&mut self) -> Option<Baz> { ... }
    fn set_foo(&mut self, foo: Foo) { ... }
    fn with_bar<R>(&self, f: impl FnOnce(&Bar) -> R) -> R { ... }
    fn with_mut_bar<R>(&self, f: impl FnOnce(&mut Bar) -> R) -> R { ... }
    fn replace_foo(&mut self, foo: Foo) -> Foo { ... }
}
```

Closure accessors such as `object.with_foo(|foo| frob(foo))` are useful when:

- Interior mutability, such as `Mutex` or `RefCell`, requires scoped access. A closure
  usually avoids leaking implementation details such as `MutexGuard<T>`.
- Changes need follow-up work or validation and a setter is unsuitable, for example
  because the property is expensive or impossible to clone.

Forward the closure's return value when appropriate. Use `with_foo()` and `with_mut_foo()`
for paired read-only and mutable accessors. A single mutating accessor may also use
`with_foo()`.

### Provide expected companion APIs

If implementing `len()`, also implement `is_empty()`.

### Respect hidden dependency APIs

Treat `#[doc(hidden)]` items as private unless you own their code.

### Commit deliberately to public `const` functions

**Rationale:** Removing `const` from a public function is a breaking change.

Mark a public function `const` only if it is expected to remain so, or if you commit to
helping clients migrate when that changes.

### Reserve operator overloading for its original purpose

Use operators for their mathematical or logical meaning, not merely as concise syntax.

### Mark cross-team public enums `#[non_exhaustive]`

**Rationale:** A required wildcard match arm lets callers continue compiling after new
variants are added. This matters most across team boundaries, where breakage can be broad.

Apply `#[non_exhaustive]` to public enums used by other teams. Exceptions:

- The variants will not change, such as `Color { Red, Green, Blue }` or
  `Visibility { Visible, Hidden }`.
- You commit to fixing all resulting breakages through a coordinated large-scale change.
- Usage is limited enough that broad breakage is not a concern.

### Mark cross-team public-field structs `#[non_exhaustive]`

**Rationale:** Adding even a private field to a struct with all-public fields breaks
external struct literals. `#[non_exhaustive]` prevents construction through struct literals
outside its crate, allowing fields to be added without that breakage.

Apply the attribute to public structs with public fields used by other teams. Exceptions:

- Fields are unlikely to change, for example a stable geometric representation.
- You commit to fixing all resulting breakages through a coordinated large-scale change.
- Users should be able to use struct update syntax, such as
  `Person { name: "foo", ..Default::default() }`.
- Usage is limited enough that broad breakage is not a concern.

### Use `#[must_use]` for meaningful return values

Mark functions `#[must_use]` when ignoring their result prevents correct use, including
pure functions, or discards an error or other critical information. A function returning
an already-annotated type such as `Result` does not need another annotation.

Mark types `#[must_use]` when they are intended as such return values. Examples include
error types and lazy computations whose intended effect requires consuming the result.

**Project rule:** Configuration builders should use consuming, fluent methods of the form
`fn field(mut self, value: T) -> Self`, marked `#[must_use]`.

### Do not execute code before `main()`

Do not arrange pre-`main()` execution, for example through the `ctor` crate.

## Implementation practices

### Make inferred types understandable

Add enough explicit types for someone familiar with the codebase to determine the types.
Prefer variable annotations over turbofish syntax. When a turbofish is required, `_`
inference markers are generally fine.

In particular, consider annotations for generic-return methods such as `.into()` and
`.parse()`, and results of complex iterator chains.

### Match values instead of checking and unwrapping

Use pattern matching to express validity to the compiler rather than checking `is_some()`
and then calling `unwrap()`.

```rust
if let Some(value) = option {
    use_value(value);
}
```

### Match known variants of non-exhaustive enums explicitly

Prefer listing all known variants. Explicitly include ignored known variants with the
wildcard arm. When only specific variants matter, prefer `if let` or `let ... else`;
`match` is acceptable when clearer.

```rust
#[non_exhaustive]
pub enum Shape {
    Circle,
    Square,
    Triangle,
}

fn process_shape(shape: Shape) {
    match shape {
        Shape::Circle => println!("It's a circle"),
        Shape::Square => println!("It's a square"),
        // Triangle and any future variants don't require any actions.
        Shape::Triangle | _ => {}
    }
}
```

For handling only circles:

```rust
if let Shape::Circle = shape {
    println!("It's a circle");
}
```

### Keep iterator chains readable

- Avoid long, complex chains, especially ones with conditionals and external mutation.
- Assign a chain to a named variable before consuming it in a `for` loop; do not construct
  the chain in the loop header.
- Usually prefer a `for` loop to `Iterator::for_each`.
- Prefer short chains when the combinator expresses substantial logic directly.

Use a loop when it makes state changes clearer:

```rust
let mut error_count = 0;
let mut processed_items = Vec::with_capacity(items.len());

for item in items {
    match transformation_that_may_fail(item) {
        Ok(val) => processed_items.push(val),
        Err(_) => error_count += 1,
    }
}
```

Name a chain before a loop:

```rust
let urgent_bugs = bug_descriptions
    .iter()
    .filter_map(|description| parse_bug_description(description).ok())
    .filter(|bug| bug.priority <= Priority::P1);

for bug in urgent_bugs {
    assign_bug(bug);
}
```

Prefer a concise combinator when it avoids reimplementing its logic:

```rust
let threshold_exceeded = values.iter().any(|&v| v > threshold);
```

### Use formatting macros for concatenation and substitution

**Rationale:** `format!` and `write!` need fewer allocations than repeated `+`
concatenation. Standard format strings avoid custom templating syntax.

Prefer `format!` or `write!` when concatenating more than two strings, for example
`format!("{a}{b}{c}")` instead of `a + b + c`, and when substituting into a literal
instead of using `str::replace`.

## Tests

Use the test framework appropriate to the codebase.

### Give test assertions useful context

In tests, consider an assertion's optional format string, particularly when it can replace
an explanatory comment, log relevant values, or distinguish API misuse from an
implementation bug. Do not merely repeat the condition.

Source examples, restricted here to test code:

```rust
assert!(
    values.is_sorted(),
    "`values` is populated from a `PriorityQueue`, so we expect it to be sorted"
);
assert!(refund > 0, "Refund must be positive, got {refund:?}");
```
