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
        // Diff hunk actions
        StageHunk,
        UnstageHunk,
        RevertHunk,
        ToggleHunkDiff,
        ExpandAllDiffHunks,
        CollapseAllDiffHunks,
        // Navigation
        GoToHunk,
        GoToPrevHunk,
    ]
);
