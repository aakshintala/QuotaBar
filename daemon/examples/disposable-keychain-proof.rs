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
            Ok(()) => {
                println!(
                    "Disposable Keychain proof passed: unlocked raw modify succeeded with exact read-back; the read-denied fixture was not modified; locked raw modify had a recognized no-UI outcome and exact post-unlock data verification."
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
