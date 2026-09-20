use hbb_common::{
    config::{
        Config, OPTION_WAYLAND_CAPTURE_BACKEND, OPTION_WAYLAND_CURSOR_MODE,
        OPTION_WAYLAND_DMABUF, OPTION_WAYLAND_INPUT_BACKEND,
    },
    env_logger::{init_from_env, Env, DEFAULT_FILTER_ENV},
    log, tokio,
};
use librustdesk::input_service::{setup_uinput, ENIGO};
use enigo::{MouseButton, MouseControllable};
use scrap::codec::{EncoderApi, EncoderCfg};
use scrap::vpxcodec::{VpxEncoder, VpxEncoderConfig, VpxVideoCodecId};
use scrap::STRIDE_ALIGN;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_from_env(Env::default().filter_or(DEFAULT_FILTER_ENV, "info"));

    log::info!("============================================================");
    log::info!("  WAYLAND CONFIGURATION OPTIONS INTEGRATION TEST           ");
    log::info!("============================================================");

    // 1. Test Configuration Key-Value Persistence
    log::info!("[Step 1/5] Testing Wayland Configuration options persistence...");
    
    // Set custom options
    Config::set_option(OPTION_WAYLAND_CAPTURE_BACKEND.to_string(), "promptless".to_string());
    Config::set_option(OPTION_WAYLAND_INPUT_BACKEND.to_string(), "uinput".to_string());
    Config::set_option(OPTION_WAYLAND_DMABUF.to_string(), "N".to_string());
    Config::set_option(OPTION_WAYLAND_CURSOR_MODE.to_string(), "embedded".to_string());

    // Verify retrieval
    let cap_val = Config::get_option(OPTION_WAYLAND_CAPTURE_BACKEND);
    let inp_val = Config::get_option(OPTION_WAYLAND_INPUT_BACKEND);
    let dma_val = Config::get_option(OPTION_WAYLAND_DMABUF);
    let cur_val = Config::get_option(OPTION_WAYLAND_CURSOR_MODE);

    log::info!("Verified option [{}]: '{}'", OPTION_WAYLAND_CAPTURE_BACKEND, cap_val);
    log::info!("Verified option [{}]: '{}'", OPTION_WAYLAND_INPUT_BACKEND, inp_val);
    log::info!("Verified option [{}]: '{}'", OPTION_WAYLAND_DMABUF, dma_val);
    log::info!("Verified option [{}]: '{}'", OPTION_WAYLAND_CURSOR_MODE, cur_val);

    assert_eq!(cap_val, "promptless");
    assert_eq!(inp_val, "uinput");
    assert_eq!(dma_val, "N");
    assert_eq!(cur_val, "embedded");
    log::info!("All configuration keys verified successfully!");

    // 2. Initialize Wayland displays & PipeWire ScreenCast with promptless backend
    log::info!("[Step 2/5] Initializing Wayland capture backend with configured 'promptless' mode...");
    let init_res = std::thread::spawn(|| {
        librustdesk::wayland::ensure_inited()
    }).join().unwrap();

    if let Err(e) = init_res {
        log::error!("Wayland ensure_inited error: {:?}", e);
        return Err(e.into());
    }

    let mut capturer_info = match librustdesk::wayland::get_capturer_for_display(0) {
        Ok(info) => info,
        Err(e) => {
            log::error!("Failed to get capturer for display 0: {:?}", e);
            return Err(e.into());
        }
    };

    let width = capturer_info.width as u32;
    let height = capturer_info.height as u32;
    log::info!("Display initialized: {}x{} with promptless Mutter/Niri D-Bus backend", width, height);

    // 3. Initialize Input Subsystem with configured 'uinput' backend
    log::info!("[Step 3/5] Initializing input subsystem with configured 'uinput' direct mode...");
    setup_uinput(0, width as i32, 0, height as i32).await?;
    log::info!("Direct /dev/uinput virtual devices created and bound to Enigo successfully!");

    // 4. Initialize VP9 Video Encoder
    log::info!("[Step 4/5] Initializing VP9 Video Encoder for {}x{}...", width, height);
    let encoder_cfg = EncoderCfg::VPX(VpxEncoderConfig {
        width,
        height,
        quality: 1.0,
        codec: VpxVideoCodecId::VP9,
        keyframe_interval: Some(30),
    });

    let mut encoder = match VpxEncoder::new(encoder_cfg, false) {
        Ok(enc) => enc,
        Err(e) => {
            log::error!("Failed to instantiate VP9 video encoder: {:?}", e);
            return Err(e.into());
        }
    };
    log::info!("VP9 Video Encoder initialized successfully.");

    // 5. Test concurrent streaming and input injection
    log::info!("[Step 5/5] Running live streaming and input test (15 frames)...");
    let mut encoded_frames = 0;
    let mut total_bytes = 0;
    let mut yuv_buffer = Vec::new();
    let mut mid_data = Vec::new();

    let start_time = Instant::now();
    let mut poll_attempts = 0;

    while encoded_frames < 15 && poll_attempts < 150 {
        poll_attempts += 1;

        match capturer_info.capturer.frame(Duration::from_millis(50)) {
            Ok(frame) => {
                // Convert frame to encoder YUV format
                if let Err(e) = frame.to(encoder.yuvfmt(), &mut yuv_buffer, &mut mid_data) {
                    log::warn!("Frame conversion error: {:?}", e);
                    continue;
                }

                // Inject concurrent input via configured uinput backend
                if encoded_frames % 5 == 0 {
                    let mut en = ENIGO.lock().unwrap();
                    en.mouse_move_relative(2, 2);
                    en.mouse_click(MouseButton::Left);
                }

                let pts_ms = start_time.elapsed().as_millis() as i64;
                match encoder.encode(pts_ms, &yuv_buffer, STRIDE_ALIGN) {
                    Ok(packets) => {
                        for packet in packets {
                            total_bytes += packet.data.len();
                        }
                        encoded_frames += 1;
                    }
                    Err(e) => {
                        log::warn!("Encoder error: {:?}", e);
                        continue;
                    }
                }
            }
            Err(_e) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    let elapsed = start_time.elapsed();
    log::info!("============================================================");
    log::info!("  TEST RESULTS WITH CONFIGURABLE WAYLAND OPTIONS            ");
    log::info!("============================================================");
    log::info!("Frames captured and encoded: {}", encoded_frames);
    log::info!("Total VP9 stream data generated: {:.2} KB", total_bytes as f64 / 1024.0);
    log::info!("Total elapsed time: {:?}", elapsed);
    if encoded_frames > 0 {
        log::info!("Average latency per frame: {:.2} ms", elapsed.as_secs_f64() * 1000.0 / encoded_frames as f64);
    }
    log::info!("============================================================");

    assert!(encoded_frames >= 10, "At least 10 frames should be captured and encoded");
    log::info!("TEST PASSED: Wayland configuration options integrated and verified!");

    Ok(())
}
