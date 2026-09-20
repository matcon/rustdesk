#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use librustdesk::*;

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
fn main() {
    if !common::global_init() {
        eprintln!("Global initialization failed.");
        return;
    }
    common::test_rendezvous_server();
    common::test_nat_type();
    common::global_clean();
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    feature = "flutter"
)))]
fn main() {
    let raw_args: Vec<String> = std::env::args().collect();
    #[cfg(target_os = "linux")]
    {
        let is_gui_arg = raw_args.iter().any(|a| a == "--cm" || a == "--install" || a == "--noinstall") 
            || (raw_args.len() <= 1);
        if is_gui_arg {
            let flutter_bin = std::path::Path::new("/usr/share/rustdesk/rustdesk");
            if flutter_bin.exists() {
                let mut cmd = std::process::Command::new(flutter_bin);
                if raw_args.len() > 1 {
                    cmd.args(&raw_args[1..]);
                }
                let status = cmd.status();
                std::process::exit(status.map(|s| s.code().unwrap_or(0)).unwrap_or(0));
            }
        }
    }
    #[cfg(all(windows, not(feature = "inline")))]
    unsafe {
        winapi::um::shellscalingapi::SetProcessDpiAwareness(2);
    }
    if let Some(args) = crate::core_main::core_main().as_mut() {
        ui::start(args);
    }
    common::global_clean();
}
