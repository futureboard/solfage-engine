//! Realtime interpolation policies. SIMD implementations can replace these
//! scalar functions after profiling without changing voice allocation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResamplingQuality {
    Draft,
    Realtime,
    High,
    Ultra,
}

#[inline]
pub fn linear(a: f32, b: f32, fraction: f32) -> f32 {
    a + (b - a) * fraction
}

#[inline]
pub fn cubic_hermite(before: f32, a: f32, b: f32, after: f32, t: f32) -> f32 {
    let c0 = a;
    let c1 = 0.5 * (b - before);
    let c2 = before - 2.5 * a + 2.0 * b - 0.5 * after;
    let c3 = 0.5 * (after - before) + 1.5 * (a - b);
    ((c3 * t + c2) * t + c1) * t + c0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_hits_endpoints() {
        assert_eq!(linear(-1.0, 1.0, 0.0), -1.0);
        assert_eq!(linear(-1.0, 1.0, 1.0), 1.0);
        assert_eq!(cubic_hermite(0.0, 1.0, 2.0, 3.0, 0.0), 1.0);
    }
}
