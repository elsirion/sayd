//! The tray's pixel-art state icons.
//!
//! The robot from sayd.sh, drawn on a 16x16 grid and rasterised here into
//! the ARGB pixmaps a StatusNotifierItem host accepts. Pixmaps rather than
//! theme names because the host must show *this* art: any stock name we
//! also published would win under the SNI spec's "prefer IconName" rule,
//! and shipping named icons into `hicolor` only works for installs that
//! actually ran an installer -- a pixmap travels over the bus and needs
//! nothing on disk.
//!
//! The head sits in the same place in every state so the icon does not
//! jump sideways in the tray when the speak waves appear or disappear.

use ksni::Icon;
use sayd_core::engine::State;

/// The site's `--acid` green: the robot while it can speak.
const ACID: u32 = 0x2bf08a;
/// The site's `--steel` grey: the robot while it cannot (muted, error).
const STEEL: u32 = 0x6b7d8c;

/// The pixmap sizes offered to the host, smallest first. 16/22/24 cover
/// the common tray heights exactly; the rest give a host that scales
/// something sharp to start from.
const SIZES: [usize; 6] = [16, 22, 24, 32, 48, 64];

/// A 16x16 one-colour sprite; row bit 15 is the leftmost column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sprite {
    rows: [u16; 16],
    rgb: u32,
}

/// The mouth is the state channel of the face: a line while speech is
/// normal, a small "o" while the robot is quiet, an "x" when it is broken.
#[derive(Clone, Copy)]
enum Mouth {
    Line,
    Dot,
    Cross,
}

fn fill(rows: &mut [u16; 16], y0: usize, y1: usize, x0: usize, x1: usize) {
    let mask = (((1u32 << (x1 - x0 + 1)) - 1) as u16) << (15 - x1);
    for row in &mut rows[y0..=y1] {
        *row |= mask;
    }
}

fn sprite(mouth: Mouth, waves: bool, rgb: u32) -> Sprite {
    let mut rows = [0u16; 16];
    // Antenna, head box and eyes.
    fill(&mut rows, 0, 1, 5, 7);
    fill(&mut rows, 2, 3, 6, 6);
    fill(&mut rows, 4, 4, 1, 11);
    fill(&mut rows, 14, 14, 1, 11);
    fill(&mut rows, 5, 13, 1, 1);
    fill(&mut rows, 5, 13, 11, 11);
    fill(&mut rows, 7, 8, 3, 4);
    fill(&mut rows, 7, 8, 8, 9);
    match mouth {
        Mouth::Line => fill(&mut rows, 11, 11, 4, 8),
        Mouth::Dot => fill(&mut rows, 11, 11, 6, 6),
        Mouth::Cross => {
            for (y, x) in [(10, 5), (10, 7), (11, 6), (12, 5), (12, 7)] {
                fill(&mut rows, y, y, x, x);
            }
        }
    }
    if waves {
        fill(&mut rows, 7, 11, 13, 13);
        fill(&mut rows, 5, 13, 15, 15);
    }
    Sprite { rows, rgb }
}

/// Which sprite the tray shows. Muted wins over everything: the icon's job
/// at a glance is "will it make sound", not "what is the engine doing".
pub fn sprite_for(state: State, muted: bool) -> Sprite {
    if muted {
        return sprite(Mouth::Dot, false, STEEL);
    }
    match state {
        State::Speaking => sprite(Mouth::Line, true, ACID),
        State::Idle => sprite(Mouth::Line, false, ACID),
        State::Paused => sprite(Mouth::Dot, false, ACID),
        State::Error => sprite(Mouth::Cross, false, STEEL),
    }
}

/// The sprite for `state`, rasterised at every size in [`SIZES`].
pub fn pixmaps(state: State, muted: bool) -> Vec<Icon> {
    let s = sprite_for(state, muted);
    SIZES.iter().map(|&n| render(&s, n)).collect()
}

/// Nearest-neighbour rasterisation to `size`, ARGB32 in network byte
/// order as the SNI spec wants. Nearest keeps the art hard-edged; at the
/// non-multiple sizes (22, 24) cells come out 1px or 2px wide, which is
/// how pixel art is supposed to scale.
fn render(s: &Sprite, size: usize) -> Icon {
    let [r, g, b] = [(s.rgb >> 16) as u8, (s.rgb >> 8) as u8, s.rgb as u8];
    let mut data = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        let row = s.rows[y * 16 / size];
        for x in 0..size {
            if row >> (15 - x * 16 / size) & 1 == 1 {
                data.extend_from_slice(&[0xff, r, g, b]);
            } else {
                data.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_state_has_a_distinct_sprite() {
        let all = [
            ("idle", sprite_for(State::Idle, false)),
            ("speaking", sprite_for(State::Speaking, false)),
            ("paused", sprite_for(State::Paused, false)),
            ("error", sprite_for(State::Error, false)),
            ("muted", sprite_for(State::Speaking, true)),
        ];
        for (i, (name_a, a)) in all.iter().enumerate() {
            for (name_b, b) in &all[i + 1..] {
                assert_ne!(a, b, "{name_a} and {name_b} render the same icon");
            }
        }
    }

    #[test]
    fn muted_overrides_the_state_sprite() {
        assert_eq!(
            sprite_for(State::Idle, true),
            sprite_for(State::Speaking, true),
            "muted must look the same whatever the engine is doing"
        );
    }

    #[test]
    fn pixmaps_are_well_formed_argb() {
        for icon in pixmaps(State::Speaking, false) {
            let n = icon.width as usize;
            assert_eq!(icon.height as usize, n);
            assert_eq!(icon.data.len(), n * n * 4, "ARGB32 needs 4 bytes/px");
            let opaque = icon
                .data
                .chunks_exact(4)
                .filter(|px| px[0] == 0xff)
                .count();
            assert!(opaque > 0, "sprite rendered fully transparent at {n}px");
            for px in icon.data.chunks_exact(4) {
                assert!(
                    px == [0, 0, 0, 0] || px == [0xff, 0x2b, 0xf0, 0x8a],
                    "unexpected pixel {px:?} at {n}px"
                );
            }
        }
    }

    #[test]
    fn exact_tray_sizes_are_offered() {
        let sizes: Vec<i32> = pixmaps(State::Idle, false).iter().map(|i| i.width).collect();
        for wanted in [16, 22, 24] {
            assert!(sizes.contains(&wanted), "no {wanted}px pixmap for the tray");
        }
    }
}
