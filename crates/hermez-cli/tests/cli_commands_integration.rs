//! Integration tests for hermez-cli library components.

#[test]
fn test_kawaii_spinner_lifecycle() {
    let mut spinner = hermez_cli::display::KawaiiSpinner::new("Loading", hermez_cli::display::SpinnerType::Dots);
    let frame = spinner.render_frame();
    assert!(!frame.is_empty());
    spinner.start();
    spinner.update_text("Still loading");
    let final_text = spinner.stop(Some("Done"));
    assert!(final_text.contains("Done"));
}

#[test]
fn test_skin_engine_has_default_skin() {
    let skin = hermez_cli::skin_engine::get_active_skin();
    assert!(!skin.name.is_empty());
}
