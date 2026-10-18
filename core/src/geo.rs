//! Distance along a GPS track.

const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Great-circle distance between two lat/lon points, in metres.
pub fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

/// Sum of consecutive segment lengths; `None` for fewer than two points.
pub fn path_length_m(points: impl Iterator<Item = (f64, f64)>) -> Option<f64> {
    let mut prev: Option<(f64, f64)> = None;
    let mut total = 0.0;
    let mut n = 0;
    for p in points {
        if let Some(q) = prev {
            total += haversine_m(q, p);
        }
        prev = Some(p);
        n += 1;
    }
    (n >= 2).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_degree_of_latitude() {
        let d = haversine_m((0.0, 0.0), (1.0, 0.0));
        assert!((d - 111_195.0).abs() < 10.0, "{d}");
        assert_eq!(path_length_m([(0.0, 0.0)].into_iter()), None);
    }
}
