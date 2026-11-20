fn main() -> std::io::Result<()> {
    emp_modules::worker(
        emp_modules::Module::Aggregate,
        env!("CARGO_PKG_VERSION"),
        option_env!("EMP_LEGACY_CORE_VERSION"),
        include_str!("../web/aggregate-settings.js"),
        emp_mod_aggregate::handle,
    )
}
