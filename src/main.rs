#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;

mod app;
mod capture;
mod engine;
mod resample;
mod shazam;
mod signature;

fn main() -> eframe::Result {
    // Headless helper for testing: identify a raw 16 kHz mono s16le file.
    let args: Vec<String> = std::env::args().collect();
    #[cfg(windows)]
    if args.len() > 1 {
        // Release builds use the GUI subsystem; reattach so CLI output is visible.
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
    if args.len() == 3 && args[1] == "--identify-raw" {
        let bytes = std::fs::read(&args[2]).expect("read file");
        let pcm: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let pcm = &pcm[..pcm.len().min(12 * signature::SAMPLE_RATE as usize)];
        let sig = signature::SignatureGenerator::generate(pcm);
        eprintln!("{} peaks over {} ms", sig.peak_count(), sig.duration_ms());
        match shazam::recognise(&sig) {
            Ok(Some(t)) => println!("{} — {}", t.artist, t.title),
            Ok(None) => println!("no match"),
            Err(e) => println!("error: {e}"),
        }
        return Ok(());
    }
    if args.len() == 2 && args[1] == "--list" {
        for s in capture::list_sources().unwrap_or_else(|e| panic!("{e}")) {
            println!("{:>8}  {}{}  [{}]", s.pid, if s.audible { "* " } else { "" }, s.title, s.app);
        }
        return Ok(());
    }
    if args.len() == 3 && args[1] == "--listen" {
        // Listen to the first source whose PID or title matches, print the first match.
        let q = &args[2];
        let src = capture::list_sources()
            .unwrap_or_else(|e| panic!("{e}"))
            .into_iter()
            .find(|s| s.pid.to_string() == *q || s.title.to_lowercase().contains(&q.to_lowercase()))
            .expect("no matching source; try --list");
        eprintln!("listening to: {} [{}]", src.title, src.app);
        let (tx, rx) = std::sync::mpsc::channel();
        let _engine = engine::Engine::start(&src, false, tx, || {}).unwrap_or_else(|e| panic!("{e}"));
        for ev in rx {
            match ev {
                engine::Event::Matched(t) => println!("{} — {}", t.artist, t.title),
                engine::Event::Finished(e) => {
                    if let Some(e) = e {
                        eprintln!("error: {e}");
                    }
                    break;
                }
                other => eprintln!("{other:?}"),
            }
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Music Identifier")
            .with_inner_size([460.0, 720.0])
            .with_min_inner_size([380.0, 520.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../packaging/icon.png")).expect("valid icon")),
        ..Default::default()
    };
    eframe::run_native("Music Identifier", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
