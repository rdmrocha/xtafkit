//! Diagnostic for the XDVDFS reader. Point it at any XISO file and it will
//! walk the directory tree and print every file's image-relative path,
//! absolute byte offset into the source, and size. Optionally extracts a
//! single named file by streaming it through `XisoImage::read_into`.
//!
//! Usage:
//!     cargo run -p fatxlib --release --example list_xiso -- <path-to-xiso>
//!     cargo run -p fatxlib --release --example list_xiso -- <path-to-xiso> \
//!         --extract <path-on-iso> <local-dest>

use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::process;
use std::time::Instant;

use fatxlib::iso::image::XisoImage;

fn human_size(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[i])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

fn print_usage_and_exit() -> ! {
    eprintln!("usage: list_xiso <path-to-xiso> [--extract <path-on-iso> <local-dest>]");
    process::exit(2);
}

fn main() {
    let mut args = env::args().skip(1);
    let Some(iso_path) = args.next() else {
        print_usage_and_exit();
    };

    let mut extract: Option<(String, PathBuf)> = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--extract" => {
                let on_iso = args.next().unwrap_or_else(|| print_usage_and_exit());
                let dest = args.next().unwrap_or_else(|| print_usage_and_exit());
                extract = Some((on_iso, PathBuf::from(dest)));
            }
            other => {
                eprintln!("unknown argument: {}", other);
                print_usage_and_exit();
            }
        }
    }

    println!("Opening {}", iso_path);
    let started = Instant::now();
    let file = File::open(&iso_path).unwrap_or_else(|e| {
        eprintln!("Error opening {}: {}", iso_path, e);
        process::exit(1);
    });
    let mut img = XisoImage::open(file).unwrap_or_else(|e| {
        eprintln!("Error parsing XDVDFS volume: {}", e);
        process::exit(1);
    });
    let layout = img
        .layout()
        .map(|l| format!("{} (0x{:08X})", l.name, l.offset))
        .unwrap_or_else(|| format!("unknown @ 0x{:08X}", img.partition_offset()));
    println!("Opened in {:?}  [layout: {}]", started.elapsed(), layout);

    let walk_started = Instant::now();
    let files = img.walk_files().unwrap_or_else(|e| {
        eprintln!("Error walking directory tree: {}", e);
        process::exit(1);
    });
    println!(
        "Walked {} files in {:?}",
        files.len(),
        walk_started.elapsed()
    );

    let mut total_bytes: u64 = 0;
    for f in &files {
        total_bytes += f.size;
        println!(
            "  {:48}  @0x{:010X}  {}",
            f.path,
            f.offset,
            human_size(f.size)
        );
    }
    println!("Total: {} files, {}", files.len(), human_size(total_bytes));

    if let Some((on_iso, dest)) = extract {
        let entry = files
            .iter()
            .find(|f| {
                f.path == on_iso
                    || f.path == on_iso.trim_start_matches('/')
                    || f.path.trim_start_matches('/') == on_iso.trim_start_matches('/')
            })
            .unwrap_or_else(|| {
                eprintln!(
                    "No file matching {:?} in the image. Path is image-relative; \
                     check the listing above for the exact spelling.",
                    on_iso
                );
                process::exit(1);
            });

        println!();
        println!(
            "Extracting {} ({}) → {}",
            entry.path,
            human_size(entry.size),
            dest.display()
        );

        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("Error creating destination directory: {}", e);
                process::exit(1);
            });
        }

        let out = File::create(&dest).unwrap_or_else(|e| {
            eprintln!("Error creating {}: {}", dest.display(), e);
            process::exit(1);
        });
        let mut out = BufWriter::new(out);

        let mut last_pct: u64 = 0;
        let mut cb = |read: u64, total: u64| {
            let pct = (read * 100).checked_div(total).unwrap_or(100);
            if pct >= last_pct + 5 || pct == 100 {
                last_pct = pct;
                eprint!("\r  {}% ({}/{})", pct, human_size(read), human_size(total));
            }
        };

        let extract_started = Instant::now();
        let n = img
            .read_into(entry, &mut out, None, Some(&mut cb))
            .unwrap_or_else(|e| {
                eprintln!("\nError reading file: {}", e);
                process::exit(1);
            });
        let elapsed = extract_started.elapsed();
        eprintln!();
        let secs = elapsed.as_secs_f64().max(1e-6);
        let throughput = (n as f64) / secs / (1024.0 * 1024.0);
        println!(
            "Wrote {} in {:?} ({:.1} MiB/s)",
            human_size(n),
            elapsed,
            throughput
        );
    }
}
