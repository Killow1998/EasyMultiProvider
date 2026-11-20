mod server;

fn main() -> std::process::ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let result = if arguments.is_empty() {
        emp_modules::worker(
            emp_modules::Module::Team,
            env!("CARGO_PKG_VERSION"),
            option_env!("EMP_LEGACY_CORE_VERSION"),
            concat!(
                include_str!("../web/team-profile.js"),
                "\n",
                include_str!("../web/team-workspace.js")
            ),
            emp_mod_team::handle,
        )
        .map_err(|error| error.to_string())
    } else {
        server::run(&arguments)
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
