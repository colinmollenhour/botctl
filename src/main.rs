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
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!("error: {error}");
            eprintln!();
            eprintln!("{}", cli::usage());
            std::process::exit(2);
        }
    }
}
