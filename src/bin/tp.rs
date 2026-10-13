//! Short command launcher. Credential access runs under the full product name.

fn main() {
    if let Err(error) = launch() {
        eprintln!("tp: could not start termplete: {error}");
        std::process::exit(1);
    }
}

fn launch() -> std::io::Result<()> {
    let executable = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "termplete.exe"
    } else {
        "termplete"
    });
    let mut command = std::process::Command::new(executable);
    command.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Replace the launcher so signals, terminal I/O, and the exit status
        // belong to termplete, including all macOS Keychain requests.
        Err(command.exec())
    }
    #[cfg(not(unix))]
    {
        std::process::exit(command.status()?.code().unwrap_or(1));
    }
}
