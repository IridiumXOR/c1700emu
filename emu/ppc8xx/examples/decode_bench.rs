//! Micro-benchmark: how long does the `powerpc` crate take to decode the ROM?
//! cargo run --release -p ppc8xx --example decode_bench -- ../roms/CISCO_bootrom_AT27C040.bin
use powerpc::{Extensions, Ins, Opcode};
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("rom path");
    let rom = std::fs::read(path).unwrap();
    let words: Vec<u32> = rom.chunks(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect();
    let rounds = 50;
    let t = Instant::now();
    let mut sink = 0u64;
    for _ in 0..rounds {
        for &w in &words {
            let ins = Ins::new(w, Extensions::none());
            sink = sink.wrapping_add(ins.op as u64);
        }
    }
    let n = (rounds * words.len()) as f64;
    let ns = t.elapsed().as_nanos() as f64 / n;
    println!("{:.1} ns per decode ({:.0} M decodes/s), sink={sink}", ns, 1000.0 / ns);
    // opcode-31 only
    let x31: Vec<u32> = words.iter().copied().filter(|w| w >> 26 == 31).collect();
    let t = Instant::now();
    for _ in 0..rounds {
        for &w in &x31 {
            sink = sink.wrapping_add(Ins::new(w, Extensions::none()).op as u64);
        }
    }
    println!("primary opcode 31: {:.1} ns per decode ({} words)", t.elapsed().as_nanos() as f64 / (rounds * x31.len()) as f64, x31.len());
    let _ = Opcode::Illegal;
}
