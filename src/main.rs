#[tokio::main]
async fn main() {
    if let Err(err) = sshpal::run().await {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
