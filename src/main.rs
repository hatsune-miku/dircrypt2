fn main() {
    if let Err(error) = dircrypt::run_cli() {
        eprintln!("[ERROR] {error:#}");
        std::process::exit(1);
    }
}
