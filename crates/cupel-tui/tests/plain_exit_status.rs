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

    #[test]
    fn configuration_warnings_go_to_stderr_even_when_model_selection_fails() {
        let root = std::env::temp_dir().join(format!(
            "cupel-plain-warnings-{}-{}",
            std::process::id(),
            cupel_core::types::now_ms()
        ));
        let (home, project) = (root.join("home"), root.join("project"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".cupel")).unwrap();
        for path in [
            home.join("settings.json"),
            home.join("models.json"),
            project.join(".cupel/models.json"),
        ] {
            std::fs::write(path, "{broken").unwrap();
        }
        std::fs::write(
            project.join(".cupel/settings.json"),
            r#"{"providers":{"fixture":"project-secret"}}"#,
        )
        .unwrap();
        std::fs::write(project.join(".cupel/bash-deny"), "[unclosed").unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_cupel"))
            .args(["--plain", "--model", "no-such-model"])
            .current_dir(&project)
            .env("CUPEL_HOME", &home)
            .env("OLLAMA_HOST", "http://127.0.0.1:9")
            .env_remove("RUST_LOG")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stdout.contains("warning:"), "{stdout}");
        assert_eq!(
            stderr.matches("warning: ignoring settings file:").count(),
            1,
            "{stderr}"
        );
        assert_eq!(
            stderr.matches("warning: ignoring models file:").count(),
            2,
            "{stderr}"
        );
        assert!(stderr.contains("invalid bash-deny pattern"), "{stderr}");
        assert!(stderr.contains("API keys belong"), "{stderr}");
        assert!(!stderr.contains("project-secret"), "{stderr}");

        // --help also consumes the offline catalog's returned warnings.
        let output = Command::new(env!("CARGO_BIN_EXE_cupel"))
            .arg("--help")
            .current_dir(&project)
            .env("CUPEL_HOME", &home)
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stdout.contains("available models:"), "{stdout}");
        assert!(!stdout.contains("warning:"), "{stdout}");
        assert_eq!(
            stderr.matches("warning: ignoring models file:").count(),
            2,
            "{stderr}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
