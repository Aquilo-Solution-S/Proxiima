use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match proxiima_mcp::run(std::env::args().skip(1)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.is_help() => {
            println!("{}", err.help_text().unwrap_or_else(proxiima_mcp::usage));
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
