//! Opens a window inside a parent window, presents a known pattern, reads
//! the pixels back from the window and compares them exactly.
//!
//! ```text
//! cargo run -p manguebyte-window --example probe
//! ```
//!
//! It prints one line per check and exits with:
//!
//! - 0 and `PASS` when every pixel read back from the window's surface is
//!   the one presented and the window answers `WM_PRINTCLIENT` with the
//!   same, or `SKIP` on a system that has no window backend yet;
//! - 1 and `FAIL` when a pixel differs or a window cannot be made;
//! - 2 and `CANNOT` when this session cannot read windows back at all
//!   (no desktop, a locked session), which says nothing about the backend.

use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    windows::run()
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    println!("SKIP: no window backend on this system yet; only the headless backend is built here");
    ExitCode::SUCCESS
}

#[cfg(windows)]
mod windows {
    use std::process::ExitCode;
    use std::time::Duration;

    use manguebyte_gfx::{IRect, Pixmap};
    use manguebyte_window::win32::testing::{self, ReadBack, TopLevel};
    use manguebyte_window::{Event, Handled, PhysicalSize, Window, WindowCx, WindowOptions};

    const WIDTH: u32 = 256;
    const HEIGHT: u32 = 160;

    /// Every pixel differs from its neighbours, and a fifth are
    /// translucent, so a swapped channel, a flipped row or an off-by-one
    /// copy all show.
    fn pattern() -> Pixmap {
        let mut data = Vec::new();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let a = if (x + y) % 5 == 0 { 128 } else { 255 };
                let scale = |c: u32| (c % 256 * a / 255) as u8;
                data.extend_from_slice(&[
                    scale(x * 9 + y),
                    scale(y * 7 + 3),
                    scale(x * 3 + y * 5),
                    a as u8,
                ]);
            }
        }
        Pixmap::from_premultiplied(WIDTH, HEIGHT, data).expect("premultiplied")
    }

    /// The colours GDI shows: premultiplied over black, opaque.
    fn shown(pixels: &Pixmap) -> Pixmap {
        let mut data = pixels.data().to_vec();
        for pixel in data.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Pixmap::from_premultiplied(pixels.width(), pixels.height(), data).expect("opaque")
    }

    fn compare(got: &Pixmap, want: &Pixmap) -> Result<(), String> {
        if (got.width(), got.height()) != (want.width(), want.height()) {
            return Err(format!(
                "read {}x{} pixels, expected {}x{}",
                got.width(),
                got.height(),
                want.width(),
                want.height()
            ));
        }
        let mut wrong = 0u64;
        let mut first = None;
        for y in 0..want.height() {
            for x in 0..want.width() {
                if got.pixel(x, y) != want.pixel(x, y) {
                    wrong += 1;
                    first.get_or_insert((x, y, got.pixel(x, y), want.pixel(x, y)));
                }
            }
        }
        match first {
            None => Ok(()),
            Some((x, y, got, want)) => Err(format!(
                "{wrong} of {} pixels differ; the first at ({x}, {y}) read {got:?}, expected {want:?}",
                WIDTH * HEIGHT
            )),
        }
    }

    pub(super) fn run() -> ExitCode {
        let parent = match TopLevel::new(WIDTH + 64, HEIGHT + 64) {
            Ok(parent) => parent,
            Err(error) => {
                println!("CANNOT: no top-level window to embed into: {error}");
                return ExitCode::from(2);
            }
        };
        let frame = pattern();
        let painted = frame.clone();
        let window = match Window::open(
            parent.parent(),
            WindowOptions::new(PhysicalSize::new(WIDTH, HEIGHT)),
            move |window: &WindowCx<'_>, event| {
                if let Event::Paint(damage) = event {
                    let _ = window.present(&painted, damage.rects());
                }
                Handled::Yes
            },
        ) {
            Ok(window) => window,
            Err(error) => {
                println!("FAIL: the child window was not made: {error}");
                return ExitCode::FAILURE;
            }
        };
        let hwnd = window.raw_handle().unwrap_or(0);
        testing::update(hwnd);
        testing::pump(Duration::from_millis(50));
        println!(
            "child window {hwnd:#x} of class {} inside {:#x}, {} pixels presented",
            testing::window_class_name(),
            parent.handle().unwrap_or(0),
            window.presented_pixels()
        );
        let want = shown(&frame);

        // The checks below read pixels; first make sure this session can.
        let outside = IRect::new(WIDTH as i32 + 4, HEIGHT as i32 + 4, 56, 56);
        if let Err(why) = testing::can_read_back(&parent, outside) {
            println!("CANNOT: this session cannot read windows back from the screen: {why}");
            return ExitCode::from(2);
        }
        let what = "the window's surface (BitBlt from its DC)";
        match testing::read_back(&window.cx(), ReadBack::Screen) {
            Ok(got) => match compare(&got, &want) {
                Ok(()) => println!("ok: {what} read back {WIDTH}x{HEIGHT} pixels exactly"),
                Err(why) => {
                    println!("FAIL: {what}: {why}");
                    return ExitCode::FAILURE;
                }
            },
            Err(error) => {
                println!("FAIL: {what}: {error}");
                return ExitCode::FAILURE;
            }
        }
        // Not a pixel check: the window's own answer from its back buffer.
        match testing::read_back(&window.cx(), ReadBack::PrintClient) {
            Ok(got) => match compare(&got, &want) {
                Ok(()) => println!("ok: WM_PRINTCLIENT answered with the back buffer"),
                Err(why) => {
                    println!("FAIL: WM_PRINTCLIENT: {why}");
                    return ExitCode::FAILURE;
                }
            },
            Err(error) => {
                println!("FAIL: WM_PRINTCLIENT: {error}");
                return ExitCode::FAILURE;
            }
        }

        drop(window);
        if testing::class_registered() {
            println!("FAIL: the window class is still registered after the last window closed");
            return ExitCode::FAILURE;
        }
        println!("ok: the window class went with the last window");
        drop(parent);
        println!("PASS");
        ExitCode::SUCCESS
    }
}
