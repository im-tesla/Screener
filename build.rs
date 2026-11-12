use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufReader, Read},
    path::PathBuf,
};

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    for (name, key) in [("ffmpeg.exe", "FFMPEG"), ("libmpv-2.dll", "MPV")] {
        let path = PathBuf::from("vendor").join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let size = fs::metadata(&path)
            .unwrap_or_else(|_| {
                panic!("Missing vendor/{name}. Run scripts/fetch-runtime.ps1 first.")
            })
            .len();
        let mut hasher = Sha256::new();
        let mut reader = BufReader::new(File::open(&path).unwrap());
        let mut buffer = [0u8; 65536];
        loop {
            let n = reader.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let hash = format!("{:x}", hasher.finalize());
        println!("cargo:rustc-env=SCREENER_{key}_SHA={hash}");
        println!("cargo:rustc-env=SCREENER_{key}_SIZE={size}");
        let compressed = out.join(format!("{name}.zst"));
        let stamp = out.join(format!("{name}.sha"));
        if !compressed.is_file() || fs::read_to_string(&stamp).unwrap_or_default() != hash {
            zstd::stream::copy_encode(
                BufReader::new(File::open(path).unwrap()),
                File::create(compressed).unwrap(),
                6,
            )
            .unwrap();
            fs::write(stamp, hash).unwrap();
        }
    }
}
