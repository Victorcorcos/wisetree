use std::process::ExitCode;

fn main() -> ExitCode {
    match wisetree::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Fatal: {err}");
            ExitCode::from(1)
        }
    }
}
