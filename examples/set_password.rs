use hbb_common::config::Config;

fn main() {
    let pwd = std::env::args().nth(1).unwrap_or_else(|| "12345678".to_string());
    if Config::set_permanent_password(&pwd) {
        println!("Permanent password successfully set to: {}", pwd);
    } else {
        eprintln!("Failed to set permanent password");
    }
}
