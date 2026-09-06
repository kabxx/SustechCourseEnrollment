use sustech_course_enrollment::tui;

#[tokio::main]
async fn main() {
    if let Err(error) = tui::run().await {
        eprintln!("[ERROR] {error}");
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            eprintln!("[CAUSE] {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}
