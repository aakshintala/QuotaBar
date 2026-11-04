use std::process::ExitCode;
use std::time::Instant;

use chrono::Utc;
use quotabard::feed::ErrorCode;
use quotabard::providers::opencode_http::OpenCodeHttpProbe;
use quotabard::runtime_state::RuntimeState;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--run-live-check"))
        || args.next().is_some()
    {
        eprintln!("No provider request was made. Pass --run-live-check to opt in.");
        return ExitCode::from(2);
    }

    run_live_check().await
}

async fn run_live_check() -> ExitCode {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return report("configuration_error", 0, None);
    };
    if !home.is_absolute() {
        return report("configuration_error", 0, None);
    }

    let mut state = match RuntimeState::open(&home) {
        Ok(state) => state,
        Err(_) => return report("configuration_error", 0, None),
    };
    let config = match state.load_config() {
        Ok(config) => config,
        Err(_) => return report("configuration_error", 0, None),
    };
    if config
        .disabled_providers
        .iter()
        .any(|id| id == "opencode-go")
    {
        return report("provider_disabled", 0, None);
    }

    let now = Utc::now();
    let cooldown = match state.load_cooldown("opencode-go", now) {
        Ok(cooldown) => cooldown,
        Err(_) => return report("persistence_error", 0, None),
    };
    if cooldown
        .not_before
        .is_some_and(|not_before| not_before > now)
    {
        return report("cooldown", 0, None);
    }

    let xdg_data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute());
    let probe = match OpenCodeHttpProbe::new(home, xdg_data_home) {
        Ok(probe) => probe,
        Err(_) => return report("provider_error", 0, None),
    };

    let started = Instant::now();
    let outcome = probe.probe_at(now).await;
    let elapsed_ms = started.elapsed().as_millis();
    let completed_at = Utc::now();
    if let Some(retry_after) = outcome
        .retry_after
        .filter(|deadline| *deadline > completed_at)
        && state
            .extend_cooldown("opencode-go", retry_after, completed_at)
            .is_err()
    {
        return report("persistence_error", elapsed_ms, outcome.http_status);
    }

    let result = match outcome.result {
        Ok(_observation) => "success",
        Err(error) => error_code(error),
    };
    report(result, elapsed_ms, outcome.http_status)
}

fn report(result: &str, elapsed_ms: u128, http_status: Option<u16>) -> ExitCode {
    match http_status {
        Some(status) => println!(
            "provider=opencode-go elapsed_ms={elapsed_ms} result={result} http_status={status}"
        ),
        None => println!("provider=opencode-go elapsed_ms={elapsed_ms} result={result}"),
    }
    if result == "success" {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn error_code(error: ErrorCode) -> &'static str {
    match error {
        ErrorCode::CredentialsMissing => "credentials_missing",
        ErrorCode::LoginExpired => "login_expired",
        ErrorCode::RateLimited => "rate_limited",
        ErrorCode::NetworkError => "network_error",
        ErrorCode::ProviderError => "provider_error",
        ErrorCode::KeychainBlocked => "keychain_blocked",
        ErrorCode::SubscriptionUnavailable => "subscription_unavailable",
        ErrorCode::Cooldown => "cooldown",
        ErrorCode::ConfigurationError => "configuration_error",
        ErrorCode::Busy => "busy",
        ErrorCode::PersistenceError => "persistence_error",
        ErrorCode::InternalError => "internal_error",
    }
}
