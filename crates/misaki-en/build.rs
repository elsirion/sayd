//! Compile the JSON lexicons into three artifacts per tier:
//!   <tier>.fst  - fst::Map from word -> entry index
//!   <tier>.blob - all entry payloads concatenated
//!   <tier>.idx  - (n+1) u32 LE offsets into the blob
//!
//! Payload encoding: a simple entry is its phoneme string verbatim. A
//! POS-dependent entry starts with \x01 and is TAG\x02PHONEMES segments joined
//! by \x01; an absent phoneme string (JSON null) is the empty string.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const TAGGED: u8 = 0x01;
const SEP: u8 = 0x02;

fn encode(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => {
            let mut out = String::new();
            for (tag, val) in map {
                out.push(TAGGED as char);
                out.push_str(tag);
                out.push(SEP as char);
                if let Some(s) = val.as_str() {
                    out.push_str(s);
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn build_tier(name: &str, out: &PathBuf) {
    let src = format!("data/{name}.json");
    println!("cargo:rerun-if-changed={src}");
    let text = std::fs::read_to_string(&src).unwrap_or_else(|e| panic!("{src}: {e}"));
    let json: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{src}: {e}"));

    let mut blob = Vec::new();
    let mut idx: Vec<u32> = vec![0];
    let mut builder =
        fst::MapBuilder::new(BufWriter::new(File::create(out.join(format!("{name}.fst"))).unwrap()))
            .unwrap();

    // BTreeMap iterates in sorted byte order, which is what MapBuilder requires.
    for (i, (word, value)) in json.iter().enumerate() {
        builder.insert(word.as_bytes(), i as u64).unwrap();
        blob.extend_from_slice(encode(value).as_bytes());
        idx.push(blob.len() as u32);
    }
    builder.finish().unwrap();

    File::create(out.join(format!("{name}.blob"))).unwrap().write_all(&blob).unwrap();
    let mut f = File::create(out.join(format!("{name}.idx"))).unwrap();
    for o in &idx {
        f.write_all(&o.to_le_bytes()).unwrap();
    }
    // Informational only -- not a `cargo:warning=`, which would train readers
    // to skim past real warnings. Build script stdout is captured by cargo
    // and only surfaced on failure or with -vv, which is the right amount of
    // visibility for "how big is the vendored lexicon", unlike an actual
    // problem.
    println!("{name}: {} entries, {} KB blob", json.len(), blob.len() / 1024);
}

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    build_tier("us_gold", &out);
    build_tier("us_silver", &out);
}
