use std::{
    env,
    io::{self, Write},
    time::Instant,
};
fn main() -> io::Result<()> {
    let n = env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100 * 1024 * 1024);
    let chunk = vec![b'X'; 65536];
    let mut out = io::stdout().lock();
    let start = Instant::now();
    let mut left = n;
    while left > 0 {
        let m = left.min(chunk.len());
        out.write_all(&chunk[..m])?;
        left -= m;
    }
    out.flush()?;
    eprintln!("bytes={n} elapsed_ns={}", start.elapsed().as_nanos());
    Ok(())
}
