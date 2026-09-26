#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    #[test]
    fn failed_plain_request_exits_nonzero_and_reports_on_stderr() {
        let root = std::env::temp_dir().join(format!(
            "cupel-plain-exit-{}-{}",
            std::process::id(),
            cupel_core::types::now_ms()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();

        // No configured or exported key: the provider fails before any HTTP
        // request. A real agent turn still runs, exercising plain mode's exit.
        let mut child = Command::new(env!("CARGO_BIN_EXE_cupel"))
            .args(["--plain", "--model", "gpt-6-sol"])
            .current_dir(&project)
            .env("CUPEL_HOME", root.join("home"))
            .env_remove("OPENAI_API_KEY")
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
        let output = child.wait_with_output().unwrap();

        assert!(!output.status.success(), "{:?}", output.status);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stdout.contains("error:"), "{stdout}");
        assert!(stderr.contains("error:"), "{stderr}");
        assert!(stderr.contains("openai"), "{stderr}");

        std::fs::remove_dir_all(root).unwrap();
    }
}
