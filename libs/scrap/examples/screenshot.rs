extern crate repng;
extern crate scrap;

use std::fs::File;
use std::io::ErrorKind::WouldBlock;
use std::thread;
use std::time::Duration;

use scrap::{Capturer, Display, Frame, TraitCapturer, TraitPixelBuffer};

fn main() {
    let n = Display::all().unwrap().len();
    for i in 0..n {
        record(i);
    }
}

fn get_display(i: usize) -> Display {
    Display::all().unwrap().remove(i)
}

fn record(i: usize) {
    let one_second = Duration::new(1, 0);
    let one_frame = one_second / 60;

    for d in Display::all().unwrap() {
        println!("{:?} {} {}", d.origin(), d.width(), d.height());
    }

    let display = get_display(i);
    let mut capturer = Capturer::new(display).expect("Couldn't begin capture.");
    let (w, h) = (capturer.width(), capturer.height());

    loop {
        println!("Waiting for frame from PipeWire...");
        let frame = match capturer.frame(Duration::from_millis(1000)) {
            Ok(frame) => frame,
            Err(error) => {
                if error.kind() == WouldBlock {
                    // Keep spinning.
                    thread::sleep(one_frame);
                    continue;
                } else {
                    panic!("Error: {}", error);
                }
            }
        };
        let Frame::PixelBuffer(frame) = frame else {
            return;
        };
        let buffer = frame.data();
        println!("Captured data len: {}, Saving...", buffer.len());

        let (actual_w, actual_h) = if buffer.len() == w * h * 4 {
            (w, h)
        } else if buffer.len() == 640 * 360 * 4 {
            println!("Buffer is 640x360 instead of reported {}x{}", w, h);
            (640, 360)
        } else {
            let guessed_h = 360;
            let guessed_w = buffer.len() / (guessed_h * 4);
            println!("Guessed resolution: {}x{}", guessed_w, guessed_h);
            (guessed_w, guessed_h)
        };

        // Flip the BGRA image into a RGBA image.
        let mut bitflipped = Vec::with_capacity(actual_w * actual_h * 4);
        let stride = buffer.len() / actual_h;

        for y in 0..actual_h {
            for x in 0..actual_w {
                let i = stride * y + 4 * x;
                bitflipped.extend_from_slice(&[buffer[i + 2], buffer[i + 1], buffer[i], 255]);
            }
        }

        // Save the image.
        let name = format!("screenshot{}_1.png", i);
        repng::encode(
            File::create(name.clone()).unwrap(),
            actual_w as u32,
            actual_h as u32,
            &bitflipped,
        )
        .unwrap();

        println!("Image saved to `{}`.", name);
        return;
    }
}
