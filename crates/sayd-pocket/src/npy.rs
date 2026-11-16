//! A minimal `.npy` reader: version 1 and 2 headers, little-endian `f32`,
//! C order. Anything else is an error rather than a guess.

use std::path::Path;

use crate::Error;

fn err(m: impl Into<String>) -> Error {
    Error::Npy(m.into())
}

/// The text after `'key':` in the header dict, up to the next top-level comma.
fn value_of<'a>(header: &'a str, key: &str) -> Result<&'a str, Error> {
    let pat = format!("'{key}':");
    let start = header
        .find(&pat)
        .ok_or_else(|| err(format!("header has no `{key}`")))?
        + pat.len();
    let rest = header[start..].trim_start();
    // A shape tuple holds commas of its own.
    let end = if rest.starts_with('(') {
        rest.find(')').map(|i| i + 1)
    } else {
        rest.find([',', '}'])
    }
    .ok_or_else(|| err(format!("header `{key}` is unterminated")))?;
    Ok(rest[..end].trim())
}

/// Read a little-endian, C-order `f32` array: its shape and flat contents.
pub fn read_f32(path: &Path) -> Result<(Vec<usize>, Vec<f32>), Error> {
    let raw = std::fs::read(path)?;
    if raw.len() < 10 || &raw[..6] != b"\x93NUMPY" {
        return Err(err("not an .npy file"));
    }
    let (len, start): (usize, usize) = match raw[6] {
        1 => (u16::from_le_bytes([raw[8], raw[9]]) as usize, 10),
        2 if raw.len() >= 12 => (
            u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize,
            12,
        ),
        v => return Err(err(format!("unsupported .npy version {v}"))),
    };
    let body = start
        .checked_add(len)
        .filter(|&b| b <= raw.len())
        .ok_or_else(|| err("header runs past the end of the file"))?;
    let header = std::str::from_utf8(&raw[start..body]).map_err(|_| err("header is not UTF-8"))?;

    let descr = value_of(header, "descr")?.trim_matches('\'');
    if descr != "<f4" {
        return Err(err(format!(
            "dtype `{descr}` is not little-endian f32 (`<f4`)"
        )));
    }
    if value_of(header, "fortran_order")? != "False" {
        return Err(err("fortran-order arrays are not supported"));
    }
    let shape = value_of(header, "shape")?
        .trim_matches(['(', ')'])
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| {
            d.parse::<usize>()
                .map_err(|_| err(format!("bad dimension `{d}`")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let count = shape
        .iter()
        .try_fold(1usize, |a, &d| a.checked_mul(d))
        .ok_or_else(|| err("shape overflows"))?;
    let data = &raw[body..];
    if count.checked_mul(4) != Some(data.len()) {
        return Err(err(format!(
            "shape {shape:?} wants {count} floats but the file holds {} bytes",
            data.len()
        )));
    }
    let floats = data
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok((shape, floats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn npy(header: &str, data: &[f32]) -> Vec<u8> {
        // Pad so magic(6)+version(2)+len(2)+header ends on a 64-byte boundary.
        let mut h = header.to_string();
        while !(10 + h.len() + 1).is_multiple_of(64) {
            h.push(' ');
        }
        h.push('\n');
        let mut out = b"\x93NUMPY\x01\x00".to_vec();
        out.extend((h.len() as u16).to_le_bytes());
        out.extend(h.as_bytes());
        for v in data {
            out.extend(v.to_le_bytes());
        }
        out
    }

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sayd-pocket-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn npy_reads_bos_fixture() {
        let data = [1.0, -2.5, 3.25];
        let p = write(
            "ok.npy",
            &npy(
                "{'descr': '<f4', 'fortran_order': False, 'shape': (1, 3), }",
                &data,
            ),
        );
        let (shape, v) = read_f32(&p).unwrap();
        assert_eq!(shape, [1, 3]);
        assert_eq!(v, data);
    }

    #[test]
    fn npy_rejects_big_endian_and_fortran_order() {
        let be = write(
            "be.npy",
            &npy(
                "{'descr': '>f4', 'fortran_order': False, 'shape': (1, 3), }",
                &[0.0; 3],
            ),
        );
        assert!(read_f32(&be).is_err());
        let fo = write(
            "fo.npy",
            &npy(
                "{'descr': '<f4', 'fortran_order': True, 'shape': (1, 3), }",
                &[0.0; 3],
            ),
        );
        assert!(read_f32(&fo).is_err());
    }
}
