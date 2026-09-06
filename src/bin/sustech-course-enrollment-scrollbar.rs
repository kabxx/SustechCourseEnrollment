use sustech_course_enrollment::tui::run_scrollbar_preview;

fn main() {
    if let Err(error) = run_scrollbar_preview() {
        eprintln!("[ERROR] {error}");
        std::process::exit(1);
    }
}
