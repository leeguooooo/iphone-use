//! `POST /agent/login` must answer promptly when the vault cannot be used: a
//! vault never set up answers at once, a locked one after a short wait for a
//! person at the Mac (it used to wait 90 s on `bwu unlock` with no answer).

use std::io::Write;
use std::time::{Duration, Instant};

fn fake_bwu(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut script = std::fs::File::create(&path).unwrap();
    write!(script, "#!/bin/sh\n{body}\n").unwrap();
    drop(script);
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    path
}

#[test]
fn an_unusable_vault_answers_promptly() {
    // The workspace has a crate named `core`, which `#[tokio::test]` trips on.
    tokio::runtime::Runtime::new().unwrap().block_on(unusable_vault());
}

async fn unusable_vault() {
    let dir = tempfile::tempdir().unwrap();
    let unlock_log = dir.path().join("unlock.log");
    std::env::set_var("IPHONE_USE_VAULT_UNLOCK_WAIT_SECS", "1");

    // Never set up: no account in its config.
    let bwu = fake_bwu(
        dir.path(),
        "bwu-empty",
        &format!(
            r#"case "$1" in
  unlocked) exit 1 ;;
  config) printf '%s' '{{"email":null,"base_url":null}}' ;;
  unlock) echo unlock >> '{log}'; sleep 30 ;;
  *) exit 1 ;;
esac"#,
            log = unlock_log.display()
        ),
    );
    std::env::set_var("IPHONE_USE_BWU", &bwu);
    let vault = server::login::Vault::locate().unwrap();
    let started = Instant::now();
    let error = vault.ensure_unlocked().await.unwrap_err();
    assert_eq!(error.error, "vault_not_configured");
    assert_eq!(error.status, 503);
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(!unlock_log.exists(), "no unlock prompt for a vault that does not exist");

    // Set up but locked, and nobody unlocks it: vault_locked after the wait,
    // with the prompt left open for a person to answer.
    let bwu = fake_bwu(
        dir.path(),
        "bwu-locked",
        &format!(
            r#"case "$1" in
  unlocked) exit 1 ;;
  config) printf '%s' '{{"email":"someone@example.com"}}' ;;
  unlock) echo unlock >> '{log}'; sleep 30 ;;
  *) exit 1 ;;
esac"#,
            log = unlock_log.display()
        ),
    );
    std::env::set_var("IPHONE_USE_BWU", &bwu);
    let vault = server::login::Vault::locate().unwrap();
    let started = Instant::now();
    let error = vault.ensure_unlocked().await.unwrap_err();
    assert_eq!(error.error, "vault_locked");
    assert!(error.hint.contains("unlock prompt is open"), "{}", error.hint);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    // A second call reuses the prompt still open instead of opening another.
    let _ = vault.ensure_unlocked().await.unwrap_err();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let prompts = std::fs::read_to_string(&unlock_log).unwrap_or_default();
    assert_eq!(prompts.lines().count(), 1, "{prompts}");

    // Unlocked by the person while the login waits: it goes through.
    let flag = dir.path().join("unlocked.flag");
    let bwu = fake_bwu(
        dir.path(),
        "bwu-unlocks",
        &format!(
            r#"case "$1" in
  unlocked) test -f '{flag}' ;;
  config) printf '%s' '{{"email":"someone@example.com"}}' ;;
  unlock) touch '{flag}' ;;
  *) exit 1 ;;
esac"#,
            flag = flag.display()
        ),
    );
    std::env::set_var("IPHONE_USE_BWU", &bwu);
    std::env::set_var("IPHONE_USE_VAULT_UNLOCK_WAIT_SECS", "5");
    let vault = server::login::Vault::locate().unwrap();
    // The earlier prompt (bwu-locked) is still running; it is not this
    // vault's, but one prompt at a time is the rule, so end it first.
    server::login::stop_unlock_prompt_for_tests();
    vault.ensure_unlocked().await.unwrap();
}

#[test]
fn an_account_in_the_config_means_set_up() {
    use server::login::vault_configured;
    assert!(vault_configured(br#"{"email":"a@b.c","base_url":"https://x"}"#));
    assert!(!vault_configured(br#"{"email":null}"#));
    assert!(!vault_configured(br#"{"email":"  "}"#));
    assert!(!vault_configured(br#"{}"#));
    // Output it cannot read is not taken as "missing".
    assert!(vault_configured(b"not json"));
}
