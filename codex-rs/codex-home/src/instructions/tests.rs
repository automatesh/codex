use std::fs;
use std::path::Path;

use codex_extension_api::Instructions;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::UserInstructionsProvider;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::CodexHomeUserInstructionsProvider;
use super::DEFAULT_AGENTS_MD_FILENAME;
use super::LOCAL_AGENTS_MD_FILENAME;

fn provider(home: &TempDir) -> CodexHomeUserInstructionsProvider {
    CodexHomeUserInstructionsProvider::new(
        AbsolutePathBuf::try_from(home.path().to_path_buf()).expect("absolute temp dir"),
    )
}

fn expected(
    home: &TempDir,
    filename: &str,
    text: &str,
    warnings: Vec<String>,
) -> LoadedUserInstructions {
    LoadedUserInstructions {
        instructions: Some(Instructions {
            text: text.to_string(),
            source: Some(
                AbsolutePathBuf::try_from(home.path().join(filename))
                    .expect("absolute source path"),
            ),
        }),
        warnings,
    }
}

#[cfg(unix)]
fn create_symlink_loop(path: &Path) {
    std::os::unix::fs::symlink(
        path.file_name().expect("override path should have a name"),
        path,
    )
    .expect("create symlink loop");
}

#[cfg(windows)]
fn create_symlink_loop(path: &Path) {
    std::os::windows::fs::symlink_file(
        path.file_name().expect("override path should have a name"),
        path,
    )
    .expect("create symlink loop");
}

#[tokio::test]
async fn missing_files_return_no_instructions() {
    let home = TempDir::new().expect("temp dir");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        LoadedUserInstructions::default()
    );
}

#[tokio::test]
async fn override_takes_precedence_over_default() {
    let home = TempDir::new().expect("temp dir");
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "default").expect("write default");
    fs::write(home.path().join(LOCAL_AGENTS_MD_FILENAME), "override").expect("write override");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        expected(&home, LOCAL_AGENTS_MD_FILENAME, "override", Vec::new())
    );
}

#[tokio::test]
async fn empty_override_falls_back_to_trimmed_default() {
    let home = TempDir::new().expect("temp dir");
    fs::write(home.path().join(LOCAL_AGENTS_MD_FILENAME), " \n\t").expect("write override");
    fs::write(
        home.path().join(DEFAULT_AGENTS_MD_FILENAME),
        "\n  default instructions  \n",
    )
    .expect("write default");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        expected(
            &home,
            DEFAULT_AGENTS_MD_FILENAME,
            "default instructions",
            Vec::new()
        )
    );
}

#[tokio::test]
async fn directory_override_falls_back_to_default() {
    let home = TempDir::new().expect("temp dir");
    fs::create_dir(home.path().join(LOCAL_AGENTS_MD_FILENAME)).expect("create override directory");
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "default").expect("write default");

    assert_eq!(
        provider(&home).load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", Vec::new())
    );
}

#[tokio::test]
async fn recoverable_override_read_error_warns_and_falls_back_to_default() {
    let home = TempDir::new().expect("temp dir");
    let provider = provider(&home);
    let override_path = home.path().join(LOCAL_AGENTS_MD_FILENAME);
    create_symlink_loop(&override_path);
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "default").expect("write default");
    let read_error = fs::read(&override_path).expect_err("symlink loop should not be readable");
    let warning = format!(
        "Failed to read global AGENTS.md instructions from `{}`: {read_error}",
        override_path.display()
    );

    assert_eq!(
        provider.load_user_instructions().await,
        expected(
            &home,
            DEFAULT_AGENTS_MD_FILENAME,
            "default",
            vec![warning.clone()]
        )
    );
    assert_eq!(
        provider.clone().load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", Vec::new())
    );

    fs::remove_file(&override_path).expect("remove broken symlink");
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", Vec::new())
    );
    create_symlink_loop(&override_path);
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "default", vec![warning])
    );
}

#[tokio::test]
async fn failed_refresh_retains_last_success_until_recovery_or_removal() {
    let home = TempDir::new().expect("temp dir");
    let provider = provider(&home);
    let path = home.path().join(DEFAULT_AGENTS_MD_FILENAME);
    fs::write(&path, "initial").expect("write initial instructions");
    let initial = provider.load_user_instructions().await;

    fs::remove_file(&path).expect("remove initial file");
    create_symlink_loop(&path);
    let failed = provider.load_user_instructions().await;
    assert_eq!(failed.instructions, initial.instructions);
    assert_eq!(failed.warnings.len(), 1);
    assert_eq!(provider.load_user_instructions().await, initial);

    fs::remove_file(&path).expect("remove broken symlink");
    fs::write(&path, "recovered").expect("restore instructions");
    assert_eq!(
        provider.load_user_instructions().await,
        expected(&home, DEFAULT_AGENTS_MD_FILENAME, "recovered", Vec::new()),
    );

    fs::write(&path, " \n").expect("clear instructions");
    assert_eq!(
        provider.load_user_instructions().await,
        LoadedUserInstructions::default(),
    );
    fs::remove_file(&path).expect("remove cleared instructions");
    create_symlink_loop(&path);
    let failed_after_clear = provider.load_user_instructions().await;
    assert_eq!(failed_after_clear.instructions, None);
    assert_eq!(failed_after_clear.warnings.len(), 1);
    fs::remove_file(&path).expect("remove broken symlink");
    assert_eq!(
        provider.load_user_instructions().await,
        LoadedUserInstructions::default(),
    );
}

#[tokio::test]
async fn invalid_utf8_is_lossy() {
    let home = TempDir::new().expect("temp dir");
    let path = home.path().join(DEFAULT_AGENTS_MD_FILENAME);
    let mut invalid_utf8 = b"global".to_vec();
    invalid_utf8.push(0xff);
    invalid_utf8.extend_from_slice(b" doc");
    fs::write(&path, &invalid_utf8).expect("write invalid utf-8");

    let outcome = provider(&home).load_user_instructions().await;
    assert_eq!(
        outcome,
        expected(
            &home,
            DEFAULT_AGENTS_MD_FILENAME,
            "global\u{fffd} doc",
            Vec::new()
        )
    );
}

fn protected_provider(home: &TempDir, accepted: Option<&str>) -> CodexHomeUserInstructionsProvider {
    use codex_instruction_control::ForkConfig;
    use codex_instruction_control::PolicySnapshot;
    use codex_instruction_control::PolicySource;
    use codex_instruction_control::SourceKind;
    let mut sources = vec![
        PolicySource::new(
            SourceKind::Builtin,
            "global-agents-home",
            home.path().to_string_lossy().to_string(),
        ),
        PolicySource::new(SourceKind::MainPrompt, "main", "main prompt".to_string()),
        PolicySource::new(
            SourceKind::ModelCatalog,
            "models",
            r#"{"models":[{"slug":"test","model_messages":{}}]}"#.to_string(),
        ),
    ];
    if let Some(text) = accepted {
        sources.push(PolicySource::new(
            SourceKind::Agents,
            home.path()
                .join(DEFAULT_AGENTS_MD_FILENAME)
                .to_string_lossy()
                .to_string(),
            text.to_string(),
        ));
    }
    provider(home).with_instruction_snapshot(Some(std::sync::Arc::new(
        PolicySnapshot::new(ForkConfig::default(), sources).expect("valid approved snapshot"),
    )))
}

#[tokio::test]
async fn approved_global_instructions_survive_edit_and_deletion() {
    let home = TempDir::new().expect("temp dir");
    let path = home.path().join(DEFAULT_AGENTS_MD_FILENAME);
    fs::write(&path, "accepted instructions").expect("write initial source");
    let provider = protected_provider(&home, Some("accepted instructions"));
    assert!(provider.load_user_instructions().await.warnings.is_empty());
    fs::write(&path, "unapproved replacement").expect("edit source");
    let loaded = provider.load_user_instructions().await;
    assert_eq!(loaded.instructions.unwrap().text, "accepted instructions");
    assert_eq!(loaded.warnings.len(), 1);
    fs::remove_file(&path).expect("delete source");
    assert_eq!(
        provider
            .load_user_instructions()
            .await
            .instructions
            .unwrap()
            .text,
        "accepted instructions"
    );
}

#[tokio::test]
async fn new_unapproved_override_cannot_replace_approved_default() {
    let home = TempDir::new().expect("temp dir");
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "accepted").expect("write default");
    fs::write(
        home.path().join(LOCAL_AGENTS_MD_FILENAME),
        "unapproved override",
    )
    .expect("write override");
    let loaded = protected_provider(&home, Some("accepted"))
        .load_user_instructions()
        .await;
    assert_eq!(loaded.instructions.unwrap().text, "accepted");
    assert_eq!(loaded.warnings.len(), 1);
}

#[tokio::test]
async fn unapproved_global_source_is_not_loaded() {
    let home = TempDir::new().expect("temp dir");
    fs::write(home.path().join(DEFAULT_AGENTS_MD_FILENAME), "not accepted").expect("write source");
    let loaded = protected_provider(&home, None)
        .load_user_instructions()
        .await;
    assert!(loaded.instructions.is_none());
    assert_eq!(loaded.warnings.len(), 1);
}

#[tokio::test]
async fn changing_live_codex_home_does_not_remove_accepted_global_instructions() {
    let accepted_home = TempDir::new().unwrap();
    let live_home = TempDir::new().unwrap();
    fs::write(
        accepted_home.path().join(DEFAULT_AGENTS_MD_FILENAME),
        "accepted global",
    )
    .unwrap();
    fs::write(
        live_home.path().join(DEFAULT_AGENTS_MD_FILENAME),
        "different home",
    )
    .unwrap();
    let accepted = protected_provider(&accepted_home, Some("accepted global"));
    let moved =
        provider(&live_home).with_instruction_snapshot(accepted.instruction_snapshot.clone());
    let loaded = moved.load_user_instructions().await;
    assert_eq!(loaded.instructions.unwrap().text, "accepted global");
    assert_eq!(moved.codex_home.as_path(), live_home.path());
}
