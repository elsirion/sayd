//! A seeded Gaussian source for the flow head's noise: xoshiro256++ seeded
//! through SplitMix64, turned normal by Box-Muller. It does not reproduce
//! torch's stream; it only has to be deterministic per seed.

pub struct Gauss {
    s: [u64; 4],
    /// Box-Muller makes two values at a time; this is the unused one.
    spare: Option<f32>,
}

fn splitmix64(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Gauss {
    pub fn new(seed: u64) -> Gauss {
        let mut x = seed;
        Gauss {
            s: std::array::from_fn(|_| splitmix64(&mut x)),
            spare: None,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let out = s[0].wrapping_add(s[3]).rotate_left(23).wrapping_add(s[0]);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        out
    }

    /// Uniform in (0, 1]: never 0, so its logarithm is finite.
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    fn standard(&mut self) -> f32 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let r = (-2.0 * self.uniform().ln()).sqrt();
        let theta = std::f64::consts::TAU * self.uniform();
        self.spare = Some((r * theta.sin()) as f32);
        (r * theta.cos()) as f32
    }

    /// Fill `out` with samples of N(0, std²). A `std` of 0 gives zeros.
    pub fn fill(&mut self, out: &mut [f32], std: f32) {
        if std == 0.0 {
            out.fill(0.0);
            return;
        }
        for v in out {
            *v = self.standard() * std;
        }
    }
}
