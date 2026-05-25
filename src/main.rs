fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    if let Err(e) = llme::cli::run() {
        eprintln!("llme: {e:#}");
        std::process::exit(1);
    }
}
