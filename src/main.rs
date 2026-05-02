use botctl::{app, cli};

fn main() {
    match cli::parse_args(std::env::args()) {
        Ok(command) => match app::run(command) {
            Ok(output) => {
                if !output.is_empty() {
                    println!("{output}");
                }
            }
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(error.exit_code());
            }
        },
        Err(error) => {
            eprintln!("{error}");
            let hint = cli::error_hint(&error.to_string());
            if !hint.is_empty() {
                eprintln!();
                eprintln!("{hint}");
            }
            std::process::exit(2);
        }
    }
}
