#[tokio::main]
async fn main() {
    match sshpal::run().await {
        Ok(code) => {
            if code != 0 {
                std::process::exit(code);
            }
        }
        Err(err) => {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
    }
}
