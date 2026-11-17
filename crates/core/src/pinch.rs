//! A two-finger pinch: both fingers travel along one line through a fixed
//! centre, so the content zooms without panning. Geometry is computed in runner
//! points (the screen is not square); fingers are never clamped one at a time,
//! which would change the scale and shift the centre.
use serde_json::Value;

pub const MIN_SCALE: f64 = 0.2;
pub const MAX_SCALE: f64 = 5.0;
/// A scale this close to 1 moves the fingers a few points and zooms nothing.
pub const MIN_SCALE_CHANGE: f64 = 0.05;
pub const MIN_DURATION_MS: u64 = 100;
/// The runner client bounds one timed move to 2000 ms.
pub const MAX_DURATION_MS: u64 = 2_000;
pub const DEFAULT_DURATION_MS: u64 = 500;
/// Wider finger gap, as a fraction of the shorter screen side.
pub const DEFAULT_SPREAD: f64 = 0.6;
/// The closest the two fingertips may come, runner points. Closer touches can
/// merge into one on the digitizer.
pub const MIN_GAP_PT: f64 = 40.0;

pub const HINT: &str = "pinch needs scale from 0.2 to 5 (not within 0.05 of 1), optional x/y from 0 to 1 (default 0.5), angle in degrees from -360 to 360, spread from 0.1 to 0.9 and duration_ms from 100 to 2000";

#[derive(Debug, Clone, PartialEq)]
pub struct Pinch {
    /// Normalized centre.
    pub x: f64,
    pub y: f64,
    /// End finger gap / start finger gap: below 1 zooms out, above 1 zooms in.
    pub scale: f64,
    /// Direction of the line through both fingers; 0 is horizontal.
    pub angle_deg: f64,
    /// Wider finger gap as a fraction of the shorter screen side.
    pub spread: f64,
    pub duration_ms: u64,
}

/// One finger's travel, runner points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub from: (f64, f64),
    pub to: (f64, f64),
}

/// The box every finger must stay inside, runner points.
#[derive(Debug, Clone, Copy)]
pub struct Band {
    pub x: (f64, f64),
    pub y: (f64, f64),
}

impl Pinch {
    pub fn parse(action: &Value) -> Result<Self, &'static str> {
        if action.get("points").is_some() || action.get("fingers").is_some() {
            return Err("pinch is always two fingers around x/y; it takes no points or fingers");
        }
        let number = |key: &str, default: Option<f64>| match action.get(key) {
            None => default.ok_or(HINT),
            Some(value) => value.as_f64().ok_or(HINT),
        };
        let duration_ms = match action.get("duration_ms") {
            None => DEFAULT_DURATION_MS,
            Some(value) => value.as_u64().ok_or(HINT)?,
        };
        let pinch = Self {
            x: number("x", Some(0.5))?,
            y: number("y", Some(0.5))?,
            scale: number("scale", None)?,
            angle_deg: number("angle", Some(0.0))?,
            spread: number("spread", Some(DEFAULT_SPREAD))?,
            duration_ms,
        };
        pinch.validate()?;
        Ok(pinch)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let unit = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
        if !unit(self.x)
            || !unit(self.y)
            || !self.scale.is_finite()
            || !(MIN_SCALE..=MAX_SCALE).contains(&self.scale)
            || (self.scale - 1.0).abs() < MIN_SCALE_CHANGE
            || !self.angle_deg.is_finite()
            || !(-360.0..=360.0).contains(&self.angle_deg)
            || !self.spread.is_finite()
            || !(0.1..=0.9).contains(&self.spread)
            || !(MIN_DURATION_MS..=MAX_DURATION_MS).contains(&self.duration_ms)
        {
            return Err(HINT);
        }
        Ok(())
    }

    /// Both fingers' travel on a `width`×`height` screen, kept inside `band`.
    /// The wider gap shrinks to fit the band symmetrically, so the centre and
    /// the scale are exactly what was asked for.
    pub fn strokes(
        &self,
        width: f64,
        height: f64,
        band: Band,
    ) -> Result<[Stroke; 2], &'static str> {
        self.validate()?;
        if !(width.is_finite() && height.is_finite() && width > 2.0 && height > 2.0) {
            return Err("pinch needs a known screen size");
        }
        let (cx, cy) = (self.x * width, self.y * height);
        let inside = |v: f64, (lo, hi): (f64, f64)| v >= lo && v <= hi;
        if !inside(cx, band.x) || !inside(cy, band.y) {
            return Err("pinch centre is in the screen-edge zone iOS keeps for its own gestures; move x/y toward the middle");
        }
        let radians = self.angle_deg.to_radians();
        let (ux, uy) = (radians.cos(), radians.sin());
        // Largest half-gap that keeps both fingers (c ± u·half) in the band.
        let fit = |c: f64, u: f64, (lo, hi): (f64, f64)| {
            if u.abs() < 1e-9 {
                f64::INFINITY
            } else {
                (c - lo).min(hi - c) / u.abs()
            }
        };
        let half_fit = fit(cx, ux, band.x).min(fit(cy, uy, band.y));
        let wide = (self.spread * width.min(height)).min(2.0 * half_fit);
        let ratio = self.scale.min(1.0 / self.scale);
        let narrow = wide * ratio;
        if narrow < MIN_GAP_PT {
            return Err("pinch fingers would come closer than 40pt: move x/y away from the screen edge, raise spread, or use a scale nearer 1 and repeat the pinch");
        }
        let (start, end) = if self.scale < 1.0 {
            (wide, narrow)
        } else {
            (narrow, wide)
        };
        let at = |gap: f64, sign: f64| (cx + sign * ux * gap / 2.0, cy + sign * uy * gap / 2.0);
        Ok([1.0, -1.0].map(|sign| Stroke {
            from: at(start, sign),
            to: at(end, sign),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BAND: Band = Band {
        x: (44.0, 396.0),
        y: (44.0, 912.0),
    };

    fn gap(a: (f64, f64), b: (f64, f64)) -> f64 {
        (a.0 - b.0).hypot(a.1 - b.1)
    }

    #[test]
    fn strokes_keep_the_centre_and_the_requested_scale() {
        for (scale, angle) in [
            (0.7, 0.0),
            (1.4, 0.0),
            (0.5, 45.0),
            (2.0, 90.0),
            (0.2, -30.0),
        ] {
            let p =
                Pinch::parse(&json!({"type":"pinch","x":0.5,"y":0.5,"scale":scale,"angle":angle}))
                    .unwrap();
            let [a, b] = p.strokes(440.0, 956.0, BAND).unwrap();
            let ratio = gap(a.to, b.to) / gap(a.from, b.from);
            assert!((ratio - scale).abs() < 1e-9, "scale {scale}: ratio {ratio}");
            for (pa, pb) in [(a.from, b.from), (a.to, b.to)] {
                assert!(((pa.0 + pb.0) / 2.0 - 220.0).abs() < 1e-9);
                assert!(((pa.1 + pb.1) / 2.0 - 478.0).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn a_pinch_in_starts_wide_and_a_pinch_out_ends_wide() {
        let shrink = Pinch::parse(&json!({"scale":0.7})).unwrap();
        let [a, b] = shrink.strokes(440.0, 956.0, BAND).unwrap();
        assert!((gap(a.from, b.from) - 264.0).abs() < 1e-9);
        let grow = Pinch::parse(&json!({"scale":1.4})).unwrap();
        let [a, b] = grow.strokes(440.0, 956.0, BAND).unwrap();
        assert!((gap(a.to, b.to) - 264.0).abs() < 1e-9);
    }

    #[test]
    fn near_an_edge_the_gap_shrinks_symmetrically_instead_of_clamping_one_finger() {
        let p = Pinch::parse(&json!({"x":0.2,"y":0.5,"scale":0.7})).unwrap();
        let [a, b] = p.strokes(440.0, 956.0, BAND).unwrap();
        for point in [a.from, a.to, b.from, b.to] {
            assert!(
                point.0 >= 44.0 - 1e-9 && point.0 <= 396.0 + 1e-9,
                "{point:?}"
            );
        }
        assert!(((a.from.0 + b.from.0) / 2.0 - 88.0).abs() < 1e-9);
        assert!((gap(a.to, b.to) / gap(a.from, b.from) - 0.7).abs() < 1e-9);
    }

    #[test]
    fn impossible_geometry_and_bad_values_are_refused() {
        let edge = Pinch::parse(&json!({"x":0.05,"y":0.5,"scale":0.7})).unwrap();
        assert!(edge.strokes(440.0, 956.0, BAND).is_err());
        let tight = Pinch::parse(&json!({"x":0.13,"y":0.5,"scale":0.2})).unwrap();
        assert!(tight.strokes(440.0, 956.0, BAND).is_err());
        for bad in [
            json!({}),
            json!({"scale":1.0}),
            json!({"scale":1.02}),
            json!({"scale":0.1}),
            json!({"scale":6}),
            json!({"scale":0.7,"x":1.2}),
            json!({"scale":0.7,"duration_ms":5000}),
            json!({"scale":0.7,"duration_ms":50}),
            json!({"scale":0.7,"spread":1.0}),
            json!({"scale":0.7,"points":[]}),
            json!({"scale":"0.7"}),
        ] {
            assert!(Pinch::parse(&bad).is_err(), "{bad}");
        }
    }
}
