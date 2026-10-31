//! Unit normalization. Records keep the unit they were written with (a user
//! can switch km↔mi in Settings mid-history); anything that aggregates across
//! records converts to one canonical unit per dimension first.

/// Convert to the canonical unit for its dimension: distances → `m`,
/// energy → `kcal`, durations → `min`, volumes → `mL`. Mass has two: body-scale
/// units → `kg` and food-scale units → `g`, so a protein series reads 50 g, not
/// 0.05 kg. `mg` and `mcg` stay as they are: HealthKit records each nutrient in
/// one of them (sodium 800 mg, not 0.8 g). Unknown units pass through
/// unchanged; aggregates that meet two different canonical units must refuse
/// rather than mix them.
pub fn canonical(value: f64, unit: &str) -> (f64, &str) {
    match unit {
        "m" => (value, "m"),
        "km" => (value * 1000.0, "m"),
        "cm" => (value / 100.0, "m"),
        "mi" => (value * 1609.344, "m"),
        "ft" => (value * 0.3048, "m"),
        "yd" => (value * 0.9144, "m"),
        "kcal" | "Cal" => (value, "kcal"),
        "kJ" => (value / 4.184, "kcal"),
        "cal" => (value / 1000.0, "kcal"),
        "s" => (value / 60.0, "min"),
        "min" => (value, "min"),
        "hr" => (value * 60.0, "min"),
        "kg" => (value, "kg"),
        "lb" => (value * 0.453_592_37, "kg"),
        "st" => (value * 6.350_293_18, "kg"),
        "g" => (value, "g"),
        "oz" => (value * 28.349_523_125, "g"),
        "mL" => (value, "mL"),
        "dL" => (value * 100.0, "mL"),
        "L" => (value * 1000.0, "mL"),
        "fl_oz_us" => (value * 29.573_529_562_5, "mL"),
        "fl_oz_imp" => (value * 28.413_062_5, "mL"),
        "cup_us" => (value * 236.588_236_5, "mL"),
        "cup_imp" => (value * 284.130_625, "mL"),
        "pt_us" => (value * 473.176_473, "mL"),
        "pt_imp" => (value * 568.261_25, "mL"),
        other => (value, other),
    }
}

/// Types whose samples add up over time (steps, distance, energy, …): their
/// natural daily/weekly aggregate is a sum, subject to the one-source rule
/// (INVARIANTS #2). Everything else is a level (heart rate, HRV, VO2max) and
/// averages.
pub fn is_cumulative(identifier: &str) -> bool {
    const CUMULATIVE: &[&str] = &[
        "StepCount",
        "DistanceWalkingRunning",
        "DistanceCycling",
        "DistanceSwimming",
        "DistanceWheelchair",
        "DistanceDownhillSnowSports",
        "ActiveEnergyBurned",
        "BasalEnergyBurned",
        "FlightsClimbed",
        "AppleExerciseTime",
        "AppleStandTime",
        "AppleMoveTime",
        "SwimmingStrokeCount",
        "PushCount",
        "NikeFuel",
        "DietaryEnergyConsumed",
        "DietaryWater",
        "TimeInDaylight",
    ];
    let short = short_type(identifier);
    CUMULATIVE.contains(&short)
}

/// `HKQuantityTypeIdentifierStepCount` → `StepCount`.
pub fn short_type(identifier: &str) -> &str {
    for p in [
        "HKQuantityTypeIdentifier",
        "HKCategoryTypeIdentifier",
        "HKDataType",
        "HKCorrelationTypeIdentifier",
    ] {
        if let Some(s) = identifier.strip_prefix(p) {
            return s;
        }
    }
    identifier
}

/// `HKWorkoutActivityTypeRunning` → `Running`.
pub fn short_activity(activity_type: &str) -> &str {
    activity_type
        .strip_prefix("HKWorkoutActivityType")
        .unwrap_or(activity_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_to_canonical() {
        assert_eq!(canonical(1.5, "km"), (1500.0, "m"));
        assert_eq!(canonical(120.5, "Cal"), (120.5, "kcal"));
        assert!((canonical(1.0, "mi").0 - 1609.344).abs() < 1e-9);
        assert!((canonical(4.184, "kJ").0 - 1.0).abs() < 1e-9);
        assert_eq!(canonical(52.0, "count/min"), (52.0, "count/min"));
        assert!((canonical(176.0, "lb").0 - 79.832_257).abs() < 1e-5);
        assert_eq!(canonical(176.0, "lb").1, "kg");
        assert_eq!(canonical(800.0, "mg"), (800.0, "mg"));
        assert!((canonical(8.0, "fl_oz_us").0 - 236.588_236_5).abs() < 1e-9);
        assert_eq!(canonical(1.5, "L"), (1500.0, "mL"));
    }

    #[test]
    fn names() {
        assert_eq!(short_type("HKQuantityTypeIdentifierStepCount"), "StepCount");
        assert_eq!(
            short_type("HKCategoryTypeIdentifierSleepAnalysis"),
            "SleepAnalysis"
        );
        assert_eq!(short_activity("HKWorkoutActivityTypeRunning"), "Running");
        assert!(is_cumulative("HKQuantityTypeIdentifierStepCount"));
        assert!(!is_cumulative("HKQuantityTypeIdentifierHeartRate"));
    }
}
