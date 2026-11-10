fn main() -> anyhow::Result<()> {
    axe::compat::import_env();
    axe::cli::run()
}
