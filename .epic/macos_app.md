# Building a macOS App in Rust with GPUI

> A field guide to the essential primitives, dependencies, and architectural decisions
> required before writing your first line of application code.

---

## Prologue: What GPUI Is (and Isn't)

GPUI is not a cross-platform widget toolkit. It is not a WebView wrapper. It is not
AppKit with a Rust skin. It is **Zed's internal application framework** — a
Metal-backed, entity-ownership-driven UI engine that erases the macOS platform layer
entirely, giving you a single Rust-native model for windows, layout, rendering, and
input.

The consequence of that design choice is significant: you never write Objective-C, never
touch Cocoa, never reach for `winit` or `glutin`. You write `Entity<T>`, implement
`Render`, call `cx.notify()`, and GPUI handles the rest.

The trade-off is equally significant: GPUI is **not a stable public API**. It is an
internal tool published openly because Zed itself is open source. It breaks on upstream
changes without notice. Pin your dependency to a specific commit and treat forward-ports
as a recurring maintenance cost.

---

## Act I: The Absolute Minimum

### The Dependency

GPUI is not published to crates.io. You pull it directly from the Zed monorepo as a git
dependency:

```toml
[dependencies]
gpui = { git = "https://github.com/zed-industries/zed", rev = "<commit-sha>" }
```

Pin the `rev`. Never track `main` in a working project.

### The Entry Point

The entire macOS run loop is three lines:

```rust
fn main() {
    App::new().run(|cx| {
        cx.open_window(WindowOptions::default(), |window, cx| {
            cx.new(|cx| MyRootView::new(window, cx))
        });
    });
}
```

`App::new()` boots the Metal renderer, initializes the Taffy layout engine, and
configures the Cocoa event loop. `.run()` hands control to the platform and never
returns. Everything your application does happens inside that closure or in entities
spawned from it.

### The Root View Contract

Every renderable type implements `Render`:

```rust
pub trait Render: 'static + Sized {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement;
}
```

Your root view is just a struct that implements this trait. There is no component
lifecycle, no mount/unmount hooks, no virtual DOM. When GPUI needs a frame, it calls
`render()`. When your state changes, you call `cx.notify()` to schedule the next frame.
That is the entire contract.

---

## Act II: What GPUI Provides for Free

You do not wire these up. They come with the dependency.

### Metal-Backed Renderer

GPUI owns a Metal render pipeline. Scene construction, GPU resource management, and
frame submission are handled internally. You compose elements; GPUI draws them. You
never touch a shader or a command buffer.

### Taffy Layout Engine

Layout is computed by **Taffy**, a pure-Rust implementation of CSS Flexbox and Grid.
The API is Tailwind-inspired:

```rust
h_flex()                    // display: flex; flex-direction: row
    .gap_2()                // gap: 0.5rem
    .p_4()                  // padding: 1rem
    .child(label)
    .child(button)
```

`div()`, `h_flex()`, and `v_flex()` are your universal building blocks. Everything
composes.

### Core Text Integration

Font loading, shaping, subpixel rendering, and line layout run through Core Text.
You supply font data (via assets — see Act III); GPUI handles the rest.

### Input Handling

Keyboard events, mouse events, trackpad gestures, scroll, drag-and-drop — all
normalized and delivered through GPUI's event system. You attach handlers with
`.on_click()`, `.on_key_down()`, `.on_drag()`, `.on_drop()`. The platform
differences between input devices are invisible to your code.

### Accessibility

GPUI constructs a basic macOS accessibility tree automatically from your element
hierarchy. No manual `AXUIElement` wrangling required.

---

## Act III: What You Wire Up Yourself

These are not optional. Without them, you have a black window.

### Asset Loading

GPUI requires an implementation of `AssetSource` to resolve fonts, icons, and images
by path. The standard approach is to bake assets into the binary at compile time using
`rust-embed`:

```rust
#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<[u8]>>> {
        Self::get(path)
            .map(|f| Some(f.data))
            .ok_or_else(|| anyhow!("asset not found: {path}"))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

App::new().with_assets(Assets).run(|cx| { ... });
```

You need at minimum **one font embedded** in your assets. GPUI has no fallback system
font loader. Without a font, nothing renders.

### A Color / Theme System

GPUI's styling primitives (`.bg()`, `.text_color()`, `.border_color()`) take `Hsla`
or `Rgba` values directly. There is no built-in theme engine. Zed's `theme` crate
exists but is enormous and tightly coupled to the full Zed workspace.

Start minimal:

```rust
pub struct Theme {
    pub background:       Hsla,
    pub surface:          Hsla,
    pub text_primary:     Hsla,
    pub text_muted:       Hsla,
    pub accent:           Hsla,
    pub border:           Hsla,
}

impl Global for Theme {}
```

Register it with `cx.set_global(Theme::default())` at startup. Access it anywhere with
`cx.global::<Theme>()`. Grow the palette as requirements demand.

### The Global Pattern for App-Wide State

Anything that isn't owned by a specific entity but needs to be accessible from any
context — settings, theme, keybinding registry, feature flags — lives as a `Global`:

```rust
cx.set_global(Settings::default());
cx.set_global(Theme::default());

// Accessible anywhere cx: &App is in scope:
let theme = cx.global::<Theme>();
```

Establish your globals in the `App::run()` closure, before opening any windows. This
is the correct initialization order.

### Keybinding Dispatch

GPUI ships a full action dispatch system, but nothing is preconfigured. You define
action types with the `actions!` macro, bind keys explicitly, and attach handlers with
`.on_action()`:

```rust
actions!(my_app, [Quit, OpenFile, Save]);

cx.bind_keys([
    KeyBinding::new("cmd-q", Quit, None),
    KeyBinding::new("cmd-o", OpenFile, None),
    KeyBinding::new("cmd-s", Save, None),
]);

// In render():
div()
    .on_action(cx.listener(|this, _: &Save, window, cx| {
        this.save(window, cx);
    }))
```

The `key_context` string on your root element controls which action scope is active.

---

## Act IV: The Practical Dependency Surface

Beyond GPUI itself, these are the crates you will need before the project is
non-trivial.

| Purpose | Crate | Why |
|---|---|---|
| Bake assets into binary | `rust-embed` | Required for font + icon loading |
| Async executor | `smol` | GPUI's executor wraps smol; zero friction with `cx.spawn()` |
| Serialization | `serde` + `serde_json` | State persistence, config, IPC |
| Error handling | `anyhow` | Standard in the Zed ecosystem |
| Ordered tree collections | `sum_tree` | Already in Zed; essential for virtualized list data |
| Compact key-value persistence | `heed` or `rocksdb` | Panel state, user preferences |

GPUI uses `smol` as its internal async executor. Using `smol` for your own background
tasks means `cx.background_spawn(async { ... })` works without any runtime
configuration.

---

## Act V: What You Never Touch

Because GPUI fully abstracts the platform, these are genuinely off the table:

- **AppKit / Cocoa** — hidden behind GPUI's platform layer
- **Core Graphics** — GPUI's scene system writes directly to Metal
- **Core Animation** — GPUI owns the display link
- **Objective-C runtime** — no `objc` crate, no `#[objc_method]`
- **`winit` or `glutin`** — GPUI has its own platform window implementation
- **Any JavaScript runtime or WebView** — not in the model

If you find yourself reaching for any of these, the architectural answer is almost
always to push the work into a `cx.background_spawn()` task and surface the result
back through an entity update.

---

## Act VI: The Sharp Edges

### Platform Support

GPUI targets **macOS (Metal)** and **Linux (Blade / Vulkan)**. Windows support exists
in the repository but is incomplete. Plan for macOS as the primary target.

### API Stability

There is none. GPUI is Zed's internal framework. The context system already underwent
one major refactor (the `ViewContext<T>` → `Context<T>` + `Window` split). The render
trait signature changed. Entity creation APIs changed. This will happen again.

The mitigation strategy: pin to a commit, do not auto-update, and schedule
forward-port work deliberately rather than reactively.

### The Mental Model Shift

The most common early mistake is bringing expectations from React, SwiftUI, or other
retained-mode frameworks. There is no reconciler, no component tree diffing, no
`useEffect`. The model is:

1. State lives in `Entity<T>` structs owned by `App`.
2. Rendering is a pure function of that state, called every dirty frame.
3. State changes happen through `entity.update(cx, |state, cx| { ... })`.
4. Re-renders are scheduled by calling `cx.notify()`.
5. Cross-entity reactivity is wired through `cx.observe()` and `cx.subscribe()`.

Once this mental model is fully internalized, the platform layer disappears. You think
in entities, subscriptions, and render functions — and GPUI handles everything below
that.

---

## Epilogue: The Initialization Checklist

Before your first meaningful frame renders, you need:

- [ ] GPUI pinned to a specific commit in `Cargo.toml`
- [ ] At minimum one font embedded via `rust-embed` and served by `AssetSource`
- [ ] A `Theme` struct registered as a `Global`
- [ ] A root entity implementing `Render` returned from `cx.open_window()`
- [ ] At least one `actions!` block with keybindings registered
- [ ] An async executor available for background work (`smol`)

Everything else — panels, virtualized lists, reactive subscriptions, split-view
layouts — builds on top of these foundations using the patterns described in this guide.