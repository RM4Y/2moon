//! Systematic Reed-Solomon erasure coding over GF(2^8) (polynomial 0x11D),
//! as used for video forward error correction.
//!
//! The encoding matrix is systematic: the first `data` rows are the identity
//! and the `parity` rows generate the recovery shards. The default parity rows
//! form a Cauchy matrix `1 / (i ^ (parity + j))`, which is what GameStream
//! hosts use on the wire (verified against captured Sunshine traffic).
//! Vandermonde-derived variants are kept for experimentation.

use std::sync::OnceLock;

struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
    mul: Box<[[u8; 256]; 256]>,
    inv: [u8; 256],
}

fn gf() -> &'static Gf {
    static GF: OnceLock<Gf> = OnceLock::new();
    GF.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        let mut mul = Box::new([[0u8; 256]; 256]);
        for a in 1..256 {
            for b in 1..256 {
                mul[a][b] = exp[log[a] as usize + log[b] as usize];
            }
        }
        let mut inv = [0u8; 256];
        for a in 1..256 {
            inv[a] = exp[255 - log[a] as usize];
        }
        Gf { exp, log, mul, inv }
    })
}

#[inline]
fn gmul(a: u8, b: u8) -> u8 {
    gf().mul[a as usize][b as usize]
}

/// `a^n` in GF(2^8).
fn gpow(a: u8, n: usize) -> u8 {
    if n == 0 {
        return 1;
    }
    if a == 0 {
        return 0;
    }
    let g = gf();
    g.exp[(g.log[a as usize] as usize * n) % 255]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixKind {
    /// Parity row `i`, column `j` = 1 / (i XOR (parity + j)). Wire-compatible default.
    Cauchy,
    /// Row `r`, column `c` = r^c (Backblaze / klauspost style).
    PowerOfRow,
    /// Row `r`, column `c` = alpha^(r*c) (Rizzo style).
    AlphaPower,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FecError {
    #[error("invalid shard counts")]
    InvalidCounts,
    #[error("not enough shards to reconstruct")]
    TooFewShards,
    #[error("shard sizes differ")]
    SizeMismatch,
    #[error("singular matrix")]
    Singular,
}

#[derive(Clone)]
pub struct ReedSolomon {
    data: usize,
    parity: usize,
    /// (data+parity) x data, row-major.
    matrix: Vec<u8>,
}

fn invert(m: &mut [u8], n: usize) -> Result<Vec<u8>, FecError> {
    // Gauss-Jordan elimination on [m | I].
    let mut inv = vec![0u8; n * n];
    for i in 0..n {
        inv[i * n + i] = 1;
    }
    for col in 0..n {
        let pivot = (col..n).find(|&r| m[r * n + col] != 0).ok_or(FecError::Singular)?;
        if pivot != col {
            for k in 0..n {
                m.swap(pivot * n + k, col * n + k);
                inv.swap(pivot * n + k, col * n + k);
            }
        }
        let p = gf().inv[m[col * n + col] as usize];
        for k in 0..n {
            m[col * n + k] = gmul(m[col * n + k], p);
            inv[col * n + k] = gmul(inv[col * n + k], p);
        }
        for r in 0..n {
            if r != col {
                let f = m[r * n + col];
                if f != 0 {
                    for k in 0..n {
                        m[r * n + k] ^= gmul(f, m[col * n + k]);
                        inv[r * n + k] ^= gmul(f, inv[col * n + k]);
                    }
                }
            }
        }
    }
    Ok(inv)
}

/// dst ^= c * src
#[inline]
fn mul_add(dst: &mut [u8], src: &[u8], c: u8) {
    if c == 0 {
        return;
    }
    if c == 1 {
        for (d, s) in dst.iter_mut().zip(src) {
            *d ^= *s;
        }
        return;
    }
    let t = &gf().mul[c as usize];
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= t[*s as usize];
    }
}

impl ReedSolomon {
    pub fn new(data: usize, parity: usize) -> Result<Self, FecError> {
        Self::with_kind(data, parity, MatrixKind::Cauchy)
    }

    pub fn with_kind(data: usize, parity: usize, kind: MatrixKind) -> Result<Self, FecError> {
        if data == 0 || data + parity > 255 {
            return Err(FecError::InvalidCounts);
        }
        let total = data + parity;
        if kind == MatrixKind::Cauchy {
            let mut matrix = vec![0u8; total * data];
            for r in 0..data {
                matrix[r * data + r] = 1;
            }
            for i in 0..parity {
                for j in 0..data {
                    matrix[(data + i) * data + j] = gf().inv[(i ^ (parity + j)) as u8 as usize];
                }
            }
            return Ok(Self { data, parity, matrix });
        }
        let mut vm = vec![0u8; total * data];
        for r in 0..total {
            for c in 0..data {
                vm[r * data + c] = match kind {
                    MatrixKind::PowerOfRow => gpow(r as u8, c),
                    MatrixKind::AlphaPower | MatrixKind::Cauchy => gf().exp[(r * c) % 255],
                };
            }
        }
        let mut top = vm[..data * data].to_vec();
        let top_inv = invert(&mut top, data)?;
        let mut matrix = vec![0u8; total * data];
        for r in 0..total {
            for c in 0..data {
                let mut acc = 0u8;
                for k in 0..data {
                    acc ^= gmul(vm[r * data + k], top_inv[k * data + c]);
                }
                matrix[r * data + c] = acc;
            }
        }
        Ok(Self { data, parity, matrix })
    }

    /// Build a systematic code from explicit parity rows (`parity` rows of `data` coefficients).
    pub fn from_parity_rows(data: usize, parity: usize, rows: &[u8]) -> Result<Self, FecError> {
        if data == 0 || data + parity > 255 || rows.len() != data * parity {
            return Err(FecError::InvalidCounts);
        }
        let mut matrix = vec![0u8; (data + parity) * data];
        for r in 0..data {
            matrix[r * data + r] = 1;
        }
        matrix[data * data..].copy_from_slice(rows);
        Ok(Self { data, parity, matrix })
    }

    pub fn data_shards(&self) -> usize {
        self.data
    }

    pub fn parity_shards(&self) -> usize {
        self.parity
    }

    /// Compute parity shards from equally sized data shards.
    pub fn encode(&self, data: &[&[u8]], parity: &mut [&mut [u8]]) -> Result<(), FecError> {
        if data.len() != self.data || parity.len() != self.parity {
            return Err(FecError::InvalidCounts);
        }
        let len = data[0].len();
        if data.iter().any(|d| d.len() != len) || parity.iter().any(|p| p.len() != len) {
            return Err(FecError::SizeMismatch);
        }
        for (p, out) in parity.iter_mut().enumerate() {
            out.fill(0);
            let row = &self.matrix[(self.data + p) * self.data..(self.data + p + 1) * self.data];
            for (d, &coef) in data.iter().zip(row) {
                mul_add(out, d, coef);
            }
        }
        Ok(())
    }

    /// Reconstruct missing data shards in place. `shards` has `data+parity`
    /// entries; `present[i]` tells whether `shards[i]` holds valid content.
    /// All shards must have the same length. Parity shards are not rebuilt.
    pub fn reconstruct_data(&self, shards: &mut [Vec<u8>], present: &[bool]) -> Result<(), FecError> {
        let total = self.data + self.parity;
        if shards.len() != total || present.len() != total {
            return Err(FecError::InvalidCounts);
        }
        if present[..self.data].iter().all(|&p| p) {
            return Ok(());
        }
        let avail: Vec<usize> = (0..total).filter(|&i| present[i]).take(self.data).collect();
        if avail.len() < self.data {
            return Err(FecError::TooFewShards);
        }
        let len = shards[avail[0]].len();
        if avail.iter().any(|&i| shards[i].len() != len) {
            return Err(FecError::SizeMismatch);
        }
        let mut sub = Vec::with_capacity(self.data * self.data);
        for &i in &avail {
            sub.extend_from_slice(&self.matrix[i * self.data..(i + 1) * self.data]);
        }
        let dec = invert(&mut sub, self.data)?;
        for missing in (0..self.data).filter(|&i| !present[i]) {
            let mut out = vec![0u8; len];
            let row = &dec[missing * self.data..(missing + 1) * self.data];
            for (k, &src) in avail.iter().enumerate() {
                mul_add(&mut out, &shards[src], row[k]);
            }
            shards[missing] = out;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(kind: MatrixKind, data: usize, parity: usize, lose: &[usize]) {
        let rs = ReedSolomon::with_kind(data, parity, kind).unwrap();
        let len = 100;
        let orig: Vec<Vec<u8>> = (0..data).map(|i| (0..len).map(|j| (i * 31 + j * 7 + 3) as u8).collect()).collect();
        let mut par: Vec<Vec<u8>> = vec![vec![0; len]; parity];
        {
            let d: Vec<&[u8]> = orig.iter().map(|v| v.as_slice()).collect();
            let mut p: Vec<&mut [u8]> = par.iter_mut().map(|v| v.as_mut_slice()).collect();
            rs.encode(&d, &mut p).unwrap();
        }
        let mut shards: Vec<Vec<u8>> = orig.iter().chain(par.iter()).cloned().collect();
        let mut present = vec![true; data + parity];
        for &l in lose {
            present[l] = false;
            shards[l] = vec![0xAA; len];
        }
        rs.reconstruct_data(&mut shards, &present).unwrap();
        assert_eq!(&shards[..data], &orig[..]);
    }

    #[test]
    fn recovers_lost_shards() {
        for kind in [MatrixKind::Cauchy, MatrixKind::PowerOfRow, MatrixKind::AlphaPower] {
            check(kind, 10, 2, &[0, 5]);
            check(kind, 10, 4, &[1, 2, 3, 11]);
            check(kind, 1, 1, &[0]);
            check(kind, 200, 50, &(0..50).collect::<Vec<_>>());
        }
    }

    #[test]
    fn too_many_losses() {
        let rs = ReedSolomon::new(4, 1).unwrap();
        let mut shards = vec![vec![0u8; 8]; 5];
        let present = [false, false, true, true, true];
        assert_eq!(rs.reconstruct_data(&mut shards, &present), Err(FecError::TooFewShards));
    }

    /// Coefficients measured from real Sunshine traffic (k=11, m=3): for each
    /// parity row, (first coef, last coef, xor of the 9 middle coefs).
    #[test]
    fn matches_sunshine_wire_coefficients() {
        let rs = ReedSolomon::new(11, 3).unwrap();
        let expected = [(244u8, 170u8, 104u8), (142, 61, 255), (1, 150, 8)];
        for (i, e) in expected.iter().enumerate() {
            let row = &rs.matrix[(11 + i) * 11..(12 + i) * 11];
            let mid = row[1..10].iter().fold(0u8, |a, b| a ^ b);
            assert_eq!((row[0], row[10], mid), *e);
        }
    }

    #[test]
    fn systematic() {
        let rs = ReedSolomon::new(5, 3).unwrap();
        for r in 0..5 {
            for c in 0..5 {
                assert_eq!(rs.matrix[r * 5 + c], (r == c) as u8);
            }
        }
    }
}
