use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--run-disposable-proof"))
        || args.next().is_some()
    {
        eprintln!("No Keychain operation was run. Pass --run-disposable-proof to opt in.");
        return ExitCode::from(2);
    }

    #[cfg(target_os = "macos")]
    {
        match quotabard::keychain_proof::native::run() {
            Ok(summary) => {
                let outcome = |value| match value {
                    quotabard::keychain_proof::LockedModifyOutcome::Success => "success",
                    quotabard::keychain_proof::LockedModifyOutcome::Refused => "refused",
                };
                println!(
                    "{}",
                    serde_json::json!({
                        "proof": "disposable-keychain",
                        "allowed_modify": outcome(summary.allowed_modify),
                        "locked_modify_attempted": summary.locked_modify_attempted,
                        "locked_modify": outcome(summary.locked_modify),
                        "locked_after": summary.locked_after,
                        "data_verified": summary.data_verified,
                    })
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Disposable Keychain proof did not pass: {error}");
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("The disposable Keychain proof runs only on macOS.");
        ExitCode::FAILURE
    }
}
