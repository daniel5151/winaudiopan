//! Application entry point.

fn main() {
    pretty_env_logger::init();

    if let Err(err) = do_main() {
        eprintln!("Error: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("Caused by: {cause}");
        }
        std::process::exit(1);
    }
}

fn do_main() -> anyhow::Result<()> {
    Ok(())
}
