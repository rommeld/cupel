use gpui::{hsla, Global, Hsla};

#[derive(Clone, Copy)]
pub struct Theme {
    pub background: Hsla,
    pub surface: Hsla,
    pub text_primary: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub border: Hsla,
    pub added: Hsla,
    pub modified: Hsla,
    pub deleted: Hsla,
}

impl Global for Theme {}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: hsla(0.0, 0.0, 0.10, 1.0),
            surface: hsla(0.0, 0.0, 0.14, 1.0),
            text_primary: hsla(0.0, 0.0, 0.90, 1.0),
            text_muted: hsla(0.0, 0.0, 0.55, 1.0),
            accent: hsla(0.58, 0.7, 0.55, 1.0),
            border: hsla(0.0, 0.0, 0.20, 1.0),
            added: hsla(0.33, 0.7, 0.55, 1.0),
            modified: hsla(0.14, 0.8, 0.55, 1.0),
            deleted: hsla(0.0, 0.7, 0.55, 1.0),
        }
    }
}
