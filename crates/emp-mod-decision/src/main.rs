fn main() -> std::io::Result<()> {
    emp_modules::worker(
        emp_modules::Module::Decision,
        env!("CARGO_PKG_VERSION"),
        option_env!("EMP_LEGACY_CORE_VERSION"),
        include_str!("../web/decision-settings.js"),
        emp_mod_decision::handle,
    )
}
