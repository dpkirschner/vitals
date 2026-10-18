# Gotchas: Apple Health export quirks

- **The file is huge.** `export.xml` is over 1 GB for a few years of Watch data, so
  it is parsed with a streaming reader straight out of the zip. Never load it
  with a DOM parser and never extract it to `/tmp`: it is health data, and `/tmp`
  is world-readable.
- **No record IDs.** The export has no UUIDs for records or workouts, so dedupe
  uses natural keys (DATA_MODEL.md). About 0.05% of records are exact duplicates
  inside a single export, and those count as `records_skipped` on the first import.
- **`device` contains a memory address.** `<<HKDevice: 0x8bd397a80>, name:…>`
  changes between exports. It is stripped by `normalize_device`.
- **Workout MetadataEntry blocks repeat.** The export writes them once before
  `<WorkoutRoute>` and again after it. The parser keeps the first value of each key
  and ignores the route's own metadata (`HKMetadataKeySyncIdentifier`).
- **Old and new workout totals.** Older exports put `totalDistance` and
  `totalEnergyBurned` on `<Workout>` itself. Newer ones use `<WorkoutStatistics>`.
  Both are stored as `workout_stats` rows.
- **Units vary across history.** The km↔mi setting, and `Cal` vs `kcal` vs `kJ`,
  differ between records. Records keep their raw unit, and every aggregate goes
  through `units::canonical` (m, kcal, min).
- **Sleep values changed in iOS 16.** Before it there was only `Asleep` and `InBed`.
  After it there are `AsleepCore`, `AsleepDeep`, `AsleepREM`, `AsleepUnspecified`
  and `Awake`. All of the asleep variants count toward `sleep_asleep_min`.
- **Nights with no Watch data** (for example while it was charging) simply have no
  sleep row. That is a gap, not a zero.
- **Samples that span midnight** count toward the local day they start on.
- **Today is partial.** An export stops mid-day. The next import fills in the rest
  of that day and rebuilds its rollup.
- **HRV beat-to-beat lists** (`InstantaneousBeatsPerMinute`) are not stored. The
  SDNN value and its metadata are.
- **Clinical records and `export_cda.xml`** are not imported.
