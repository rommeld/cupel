use gpui::actions;

actions!(
    cupel,
    [
        Quit,
        StageFile,
        UnstageFile,
        StageAll,
        UnstageAll,
        ToggleStaging,
        Commit,
        SelectNext,
        SelectPrev,
    ]
);
