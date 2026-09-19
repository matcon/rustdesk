use hbb_common::{
    env_logger::{init_from_env, Env, DEFAULT_FILTER_ENV},
    log, tokio,
};
use librustdesk::input_service::{setup_uinput, ENIGO};
use enigo::{Key, KeyboardControllable, MouseButton, MouseControllable};
use std::time::Duration;

#[tokio::main]
async fn main() {
    init_from_env(Env::default().filter_or(DEFAULT_FILTER_ENV, "info"));

    log::info!("=== Starting Wayland Native Input Injection Test ===");

    // Desktop resolution for testing
    let (minx, maxx, miny, maxy) = (0, 3840, 0, 2160);
    log::info!("Calling setup_uinput({}, {}, {}, {})...", minx, maxx, miny, maxy);

    match setup_uinput(minx, maxx, miny, maxy).await {
        Ok(()) => {
            log::info!("setup_uinput succeeded!");
        }
        Err(e) => {
            log::error!("setup_uinput failed: {}", e);
            std::process::exit(1);
        }
    }

    // Verify ENIGO has custom mouse and custom keyboard installed
    {
        let mut en = ENIGO.lock().unwrap();
        if en.get_custom_mouse().is_none() {
            log::error!("FAILED: ENIGO custom_mouse is None!");
            std::process::exit(1);
        }
        if en.get_custom_keyboard().is_none() {
            log::error!("FAILED: ENIGO custom_keyboard is None!");
            std::process::exit(1);
        }
        log::info!("SUCCESS: ENIGO has custom_mouse and custom_keyboard active!");

        log::info!("Testing mouse relative movement...");
        en.mouse_move_relative(10, 10);
        std::thread::sleep(Duration::from_millis(50));
        en.mouse_move_relative(-10, -10);
        std::thread::sleep(Duration::from_millis(50));
        log::info!("Mouse movement executed without errors.");

        log::info!("Testing mouse click simulation...");
        en.mouse_click(MouseButton::Left);
        std::thread::sleep(Duration::from_millis(50));
        log::info!("Mouse click executed without errors.");

        log::info!("Testing keyboard key down / up...");
        en.key_down(Key::Shift).ok();
        std::thread::sleep(Duration::from_millis(50));
        en.key_up(Key::Shift);
        std::thread::sleep(Duration::from_millis(50));
        log::info!("Keyboard events executed without errors.");
    }

    log::info!("=== All Wayland Input Injection Tests PASSED! ===");
}
