//! Python float semantics that decide pixel geometry in MapProxy (`round`, `//`, `int`).

/// Python `round(x, ndigits)`: correctly rounded via the shortest-exact decimal expansion
/// (Rust's fixed-precision formatting rounds the exact binary value, as CPython does).
pub fn round_nd(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.ndigits$}").parse().unwrap_or(x)
}

/// Python `round(x)` to an integer: ties to even.
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python `int(x)`: truncation toward zero.
pub fn int(x: f64) -> i64 {
    x.trunc() as i64
}

/// CPython `float_floor_div` (`a // b`), which is not always `floor(a / b)`.
pub fn floordiv(a: f64, b: f64) -> f64 {
    let m = a % b;
    let mut div = (a - m) / b;
    if m != 0.0 && (b < 0.0) != (m < 0.0) {
        div -= 1.0;
    }
    if div != 0.0 {
        let mut fd = div.floor();
        if div - fd > 0.5 {
            fd += 1.0;
        }
        fd
    } else {
        0.0_f64.copysign(a / b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_semantics() {
        assert_eq!(round_nd(2.675, 2), 2.67); // binary 2.67499999…
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(3.5), 4);
        assert_eq!(int(-2.7), -2);
        assert_eq!(floordiv(7.0, 2.0), 3.0);
        assert_eq!(floordiv(-7.0, 2.0), -4.0);
        assert_eq!(floordiv(5545984.0, 21664.0), 256.0);
    }
}
