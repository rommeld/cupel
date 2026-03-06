use cupel::app::{
    Backspace, Copy, CupelWorkspace, Cut, Delete, End, Home, Left, Paste, Quit, Right, SelectAll,
    SelectLeft, SelectRight, ShowCharacterPalette, TextInput,
};
use gpui::{App, AppContext, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, px, size};

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(300.), px(300.)), cx);
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, None),
            KeyBinding::new("delete", Delete, None),
            KeyBinding::new("left", Left, None),
            KeyBinding::new("right", Right, None),
            KeyBinding::new("shift-left", SelectLeft, None),
            KeyBinding::new("shift-right", SelectRight, None),
            KeyBinding::new("cmd-a", SelectAll, None),
            KeyBinding::new("cmd-v", Paste, None),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-x", Cut, None),
            KeyBinding::new("home", Home, None),
            KeyBinding::new("end", End, None),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
        ]);
        
        let command_output = "Loading...".into();

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| {
                    let text_input = cx.new(|cx| TextInput {
                        focus_handle: cx.focus_handle(),
                        content: "".into(),
                        placeholder: "Type here...".into(),
                        selected_range: 0..0,
                        selection_reversed: false,
                        marked_range: None,
                        last_layout: None,
                        last_bounds: None,
                        is_selecting: false,
                    });
                    cx.new(|cx| CupelWorkspace {
                        text_input,
                        command_output,
                        focus_handle: cx.focus_handle(),
                    })
                },
            )
            .expect("Expected text example.");
        let view = window.update(cx, |_, _, cx| cx.entity()).expect("");

        cx.spawn(async move |cx| {
            let output = cx
                .background_executor()
                .spawn(async {
                    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
                    rt.block_on(async {
                        tokio::process::Command::new("git")
                            .arg("log")
                            .arg("--oneline")
                            .output()
                            .await
                    })
                })
                .await;

            match output {
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stdout).to_string();
                    view.update(cx, |view, c| {
                        view.command_output = text.into();
                        c.notify();
                    })
                    .ok();
                }
                Err(e) => {
                    view.update(cx, |view, cx| {
                        view.command_output = format!("Error: {}", e).into();
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();

        window
            .update(cx, |view, window, cx| {
                window.focus(&view.text_input.read(cx).focus_handle);
                cx.activate(true);
            })
            .expect("");
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
    });
}
