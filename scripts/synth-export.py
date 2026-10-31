#!/usr/bin/env python3
"""Write a synthetic Apple Health export for integration testing.

Every value is made up. The output has the shape of a real unpacked export
(`<out>/apple_health_export/export.xml` plus `workout-routes/*.gpx`) and
deliberately contains the awkward cases real exports have, so `make dev-db`
followed by `make smoke` exercises them end to end:

- an iPhone and a Watch logging the same steps and distance (one-source rule),
  plus exact duplicate records (dedupe)
- a km -> mi switch halfway through history (distance and workout stats)
- body mass logged in kg, then in lb after a Settings change
- water logged by two apps, one in mL and one in fl_oz_us
- energy in Cal, kcal and kJ (a third-party gym app)
- blood glucose that switches from mg/dL to mmol/L partway through, which
  has no conversion and must be refused, not summed
- a spring-forward DST change and a week-long East Coast trip (offsets
  shift by three hours mid-history)
- legacy pre-iOS 16 `Asleep` values early on, stages after
- a second sleep app overlapping some nights, nights with in-bed but no
  asleep data (Watch charging), afternoon naps, and a late night that ends
  after noon
- running and cycling with GPX routes, indoor runs without, strength with no
  distance, legacy self-closing walks with `totalDistance`, and a workout
  that crosses midnight
- blood pressure correlations, mindful sessions, stand hours and activity
  summaries

Output is deterministic for a given --seed, --days and --end. The script
refuses to write inside the repository, so generated data can't be committed
by accident.
"""

import argparse
import math
import os
import random
import sys
import zipfile
from datetime import date, datetime, timedelta, timezone
from xml.sax.saxutils import escape
from zoneinfo import ZoneInfo

HOME_TZ = ZoneInfo("America/Los_Angeles")
TRIP_TZ = ZoneInfo("America/New_York")
UTC = timezone.utc
# A made-up spot for routes; nobody's home.
ROUTE_BASE = (45.0, -100.0)

WATCH = "Synthetic Watch"
PHONE = "Synthetic iPhone"
SOURCES = {
    WATCH: ("12.0", "<<HKDevice: 0xbbb2>, name:Apple Watch, manufacturer:Apple Inc., model:Watch>"),
    PHONE: ("19.0", "<<HKDevice: 0xaaa1>, name:iPhone, manufacturer:Apple Inc., model:iPhone>"),
    "Synthetic Scale": ("3.1", None),
    "Synthetic Water App": ("2.4", None),
    "Synthetic Diet App": ("7.0", None),
    "Synthetic Gym App": ("1.9", None),
    "Synthetic Sleep App": ("5.2", None),
    "Synthetic CGM": ("1.0", None),
}

GLUCOSE_MMOL = "mmol<180.1558800000541>/L"


def num(v, nd=2):
    s = "%.*f" % (nd, v)
    return s.rstrip("0").rstrip(".") if "." in s else s


def attr(v):
    return escape(str(v), {'"': "&quot;"})


def stamp(t, tz):
    return t.astimezone(tz).strftime("%Y-%m-%d %H:%M:%S %z")


class Export:
    def __init__(self, args):
        self.rng = random.Random(args.seed)
        self.days = args.days
        self.end = args.end
        self.start = args.end - timedelta(days=args.days - 1)
        self.lines = []
        self.routes = {}  # file name -> gpx text
        self.summaries = []
        # Edge-case windows, as day indexes into history.
        self.mi_from = args.days // 2
        self.lb_from = args.days * 3 // 5
        self.legacy_sleep_until = min(20, args.days // 4)
        self.trip = range(args.days * 2 // 3, args.days * 2 // 3 + 7)
        glucose_from = args.days // 3
        self.glucose = range(glucose_from, glucose_from + 30)
        self.glucose_mmol_from = glucose_from + 20
        self.late_nights = {i for i in range(10, args.days, 31)}
        self.midnight_walk = args.days // 2 + 3

    # ── writers ───────────────────────────────────────────────────────────

    def record(self, kind, ty, src, start, end, tz, value, unit=None, meta=(), indent=" "):
        version, device = SOURCES[src]
        prefix = "HKQuantityTypeIdentifier" if kind == "q" else "HKCategoryTypeIdentifier"
        a = f'type="{prefix}{ty}" sourceName="{attr(src)}" sourceVersion="{version}"'
        if device:
            a += f' device="{attr(device)}"'
        if unit:
            a += f' unit="{attr(unit)}"'
        a += (
            f' creationDate="{stamp(end, tz)}" startDate="{stamp(start, tz)}"'
            f' endDate="{stamp(end, tz)}" value="{attr(value)}"'
        )
        if not meta:
            self.lines.append(f"{indent}<Record {a}/>")
            return
        self.lines.append(f"{indent}<Record {a}>")
        for k, v in meta:
            self.lines.append(f'{indent} <MetadataEntry key="{k}" value="{attr(v)}"/>')
        self.lines.append(f"{indent}</Record>")

    def q(self, ty, src, start, end, tz, value, unit, nd=2, meta=()):
        self.record("q", ty, src, start, end, tz, num(value, nd), unit, meta)

    def sleep(self, src, start, end, tz, stage):
        self.record("c", "SleepAnalysis", src, start, end, tz, "HKCategoryValueSleepAnalysis" + stage)

    # ── one day ───────────────────────────────────────────────────────────

    def at(self, day, tz, h, m=0, s=0):
        """A local wall time on `day` in `tz`, as a UTC instant."""
        return datetime(day.year, day.month, day.day, tzinfo=tz).astimezone(UTC) + timedelta(
            hours=h, minutes=m, seconds=s
        )

    def day(self, i):
        rng = self.rng
        day = self.start + timedelta(days=i)
        tz = TRIP_TZ if i in self.trip else HOME_TZ
        weekend = day.weekday() >= 5
        level = max(0.3, rng.gauss(1.1 if weekend else 1.0, 0.25))
        watch_on = rng.random() > 0.05
        dist_unit = "mi" if i >= self.mi_from else "km"
        km_per = 1 / 1.609344 if dist_unit == "mi" else 1.0
        active_cal = 0.0
        exercise_min = 0.0
        stood = 0

        # Steps and distance: the phone and the Watch see the same walks.
        for h in range(7, 23):
            m0 = rng.randrange(0, 50)
            dur = rng.randrange(4, 10)
            start, end = self.at(day, tz, h, m0), self.at(day, tz, h, m0 + dur)
            steps = max(0, int(rng.gauss(380, 180) * level))
            if steps == 0:
                continue
            sources = [(PHONE, steps)]
            if watch_on:
                sources.append((WATCH, int(steps * rng.uniform(1.0, 1.3))))
            for src, n in sources:
                self.q("StepCount", src, start, end, tz, n, "count", 0)
                if rng.random() < 0.01:  # exact duplicate row
                    self.q("StepCount", src, start, end, tz, n, "count", 0)
                self.q("DistanceWalkingRunning", src, start, end, tz, n * 0.00075 * km_per, dist_unit, 4)
            if rng.random() < 0.2:
                self.q("FlightsClimbed", PHONE, start, end, tz, rng.randrange(1, 4), "count", 0)

        # Basal energy all day; active energy, HR, stand while the Watch is on.
        for h in range(24):
            self.q("BasalEnergyBurned", WATCH if watch_on else PHONE, self.at(day, tz, h),
                   self.at(day, tz, h + 1), tz, rng.gauss(70, 4), "kcal", 1)
        if watch_on:
            for h in range(8, 23):
                cal = max(1.0, rng.gauss(22, 9) * level)
                active_cal += cal
                self.q("ActiveEnergyBurned", WATCH, self.at(day, tz, h), self.at(day, tz, h + 1), tz, cal, "Cal", 2)
                if rng.random() < 0.75:
                    stood += 1
                    self.record("c", "AppleStandHour", WATCH, self.at(day, tz, h), self.at(day, tz, h + 1), tz,
                                "HKCategoryValueAppleStandHourStood")
                    self.q("AppleStandTime", WATCH, self.at(day, tz, h, 10), self.at(day, tz, h, 15), tz, 5, "min", 0)
                else:
                    self.record("c", "AppleStandHour", WATCH, self.at(day, tz, h), self.at(day, tz, h + 1), tz,
                                "HKCategoryValueAppleStandHourIdle")
            for k in range(7 * 4, 23 * 4):
                t = self.at(day, tz, 0, k * 15)
                meta = (("HKMetadataKeyHeartRateMotionContext", "1"),) if k % 8 == 0 else ()
                self.q("HeartRate", WATCH, t, t, tz, rng.gauss(74, 8), "count/min", 0, meta)
            self.q("RestingHeartRate", WATCH, self.at(day, tz, 0), self.at(day, tz, 20), tz,
                   rng.gauss(57 - 3 * i / self.days, 2), "count/min", 0)
            for _ in range(2):
                t = self.at(day, tz, rng.randrange(9, 21), rng.randrange(60))
                self.q("TimeInDaylight", WATCH, t, t + timedelta(minutes=20), tz, 20, "min", 0)
            if day.weekday() == 0:
                self.q("VO2Max", WATCH, self.at(day, tz, 8), self.at(day, tz, 8), tz,
                       rng.gauss(42 + 2 * i / self.days, 0.5), "mL/min·kg", 2)

        # Body mass: kg, then lb after the Settings change.
        if i % 3 == 0:
            kg = 81 - 3 * i / self.days + rng.gauss(0, 0.3)
            t = self.at(day, tz, 7, 5)
            if i >= self.lb_from:
                self.q("BodyMass", "Synthetic Scale", t, t, tz, kg / 0.45359237, "lb", 1)
            else:
                self.q("BodyMass", "Synthetic Scale", t, t, tz, kg, "kg", 2)

        # Water from two apps in two units; meals from the diet app.
        for _ in range(rng.randrange(3, 6)):
            t = self.at(day, tz, rng.randrange(8, 21), rng.randrange(60))
            self.q("DietaryWater", "Synthetic Water App", t, t, tz, rng.choice([250, 330, 500]), "mL", 0)
        if rng.random() < 0.5:
            for _ in range(rng.randrange(1, 3)):
                t = self.at(day, tz, rng.randrange(8, 21), rng.randrange(60))
                self.q("DietaryWater", "Synthetic Diet App", t, t, tz, rng.choice([8, 12, 16]), "fl_oz_us", 0)
        for h in (8, 13, 19):
            t = self.at(day, tz, h, rng.randrange(30))
            self.q("DietaryEnergyConsumed", "Synthetic Diet App", t, t, tz, rng.gauss(650, 150), "Cal", 0)
            self.q("DietaryProtein", "Synthetic Diet App", t, t, tz, rng.gauss(35, 10), "g", 1)
            self.q("DietarySodium", "Synthetic Diet App", t, t, tz, rng.gauss(800, 200), "mg", 0)

        # Glucose: mg/dL, then the CGM app switches to mmol/L (no conversion).
        if i in self.glucose:
            for h in (7, 12, 18):
                t = self.at(day, tz, h, 30)
                mgdl = rng.gauss(98, 12)
                if i >= self.glucose_mmol_from:
                    self.q("BloodGlucose", "Synthetic CGM", t, t, tz, mgdl / 18.0156, GLUCOSE_MMOL, 1)
                else:
                    self.q("BloodGlucose", "Synthetic CGM", t, t, tz, mgdl, "mg/dL", 0)

        # Blood pressure, weekly, as a correlation wrapping two records.
        if day.weekday() == 2:
            t = self.at(day, tz, 8, 15)
            self.lines.append(
                f' <Correlation type="HKCorrelationTypeIdentifierBloodPressure" sourceName="{PHONE}" '
                f'sourceVersion="19.0" creationDate="{stamp(t, tz)}" startDate="{stamp(t, tz)}" '
                f'endDate="{stamp(t, tz)}">'
            )
            for ty, v in (("BloodPressureSystolic", rng.gauss(118, 6)), ("BloodPressureDiastolic", rng.gauss(76, 4))):
                self.record("q", ty, PHONE, t, t, tz, num(v, 0), "mmHg", indent="  ")
            self.lines.append(" </Correlation>")

        if i % 4 == 1:
            t = self.at(day, tz, 21, 30)
            self.record("c", "MindfulSession", PHONE, t, t + timedelta(minutes=10), tz, "HKCategoryValueNotApplicable")

        cal, mins = self.workouts(i, day, tz, dist_unit, km_per)
        active_cal += cal
        exercise_min += mins
        if watch_on:
            brisk = rng.randrange(0, 15)
            if brisk:
                t = self.at(day, tz, 12, 10)
                self.q("AppleExerciseTime", WATCH, t, t + timedelta(minutes=brisk), tz, brisk, "min", 0)
                exercise_min += brisk
        self.night(i, day, tz, watch_on, weekend)
        if rng.random() < 0.11 and watch_on:  # afternoon nap
            s = self.at(day, tz, 14, rng.randrange(0, 30))
            self.sleep(WATCH, s, s + timedelta(minutes=rng.randrange(25, 55)), tz, "AsleepUnspecified")

        self.summaries.append(
            f' <ActivitySummary dateComponents="{day.isoformat()}" activeEnergyBurned="{num(active_cal, 1)}" '
            f'activeEnergyBurnedGoal="500" activeEnergyBurnedUnit="Cal" appleMoveTime="0" appleMoveTimeGoal="0" '
            f'appleExerciseTime="{num(exercise_min, 0)}" appleExerciseTimeGoal="30" '
            f'appleStandHours="{stood}" appleStandHoursGoal="12"/>'
        )

    # ── sleep ─────────────────────────────────────────────────────────────

    def night(self, i, day, tz, watch_on, weekend):
        """The night starting on the evening of `day`."""
        rng = self.rng
        if i in self.late_nights:
            bed = self.at(day, tz, 27, 30)  # 03:30, waking after noon
            asleep_h = 9.1
        else:
            bed = self.at(day, tz, 22, 45 + rng.randrange(0, 75) + (45 if weekend else 0))
            asleep_h = rng.uniform(6.5, 8.3)
        wake = bed + timedelta(hours=asleep_h)
        self.sleep(PHONE, bed - timedelta(minutes=10), wake + timedelta(minutes=12), tz, "InBed")
        if not watch_on:
            return  # in bed, but nothing recorded asleep
        if i < self.legacy_sleep_until:
            self.sleep(WATCH, bed, wake, tz, "Asleep")
        else:
            t = bed
            cycle = 0
            while t < wake:
                deep = max(5, 25 - 5 * cycle)
                rem = 15 + 6 * cycle
                for stage, mins in (("AsleepCore", 35), ("AsleepDeep", deep), ("AsleepCore", 15), ("AsleepREM", rem)):
                    end = min(wake, t + timedelta(minutes=mins))
                    if end > t:
                        self.sleep(WATCH, t, end, tz, stage)
                    t = end
                if t < wake and rng.random() < 0.3:
                    end = min(wake, t + timedelta(minutes=rng.randrange(2, 8)))
                    self.sleep(WATCH, t, end, tz, "Awake")
                    t = end
                cycle += 1
        if rng.random() < 0.2:  # a second sleep app overlapping the night
            self.sleep("Synthetic Sleep App", bed + timedelta(minutes=5), wake - timedelta(minutes=8), tz,
                       "AsleepUnspecified")
        for k in range(int(asleep_h * 2)):
            t = bed + timedelta(minutes=30 * k + 15)
            self.q("HeartRate", WATCH, t, t, tz, rng.gauss(55, 4), "count/min", 0)
        for k in range(3):
            t = bed + timedelta(hours=1 + 2 * k)
            self.q("HeartRateVariabilitySDNN", WATCH, t, t + timedelta(minutes=1), tz, max(10, rng.gauss(46, 11)),
                   "ms", 1, (("HKAlgorithmVersion", "2"),))
        mid = bed + timedelta(hours=asleep_h / 2)
        self.q("RespiratoryRate", WATCH, mid, mid, tz, rng.gauss(14.5, 0.8), "count/min", 1)
        self.q("OxygenSaturation", WATCH, mid, mid, tz, min(1.0, rng.gauss(0.97, 0.01)), "%", 2)

    # ── workouts ──────────────────────────────────────────────────────────

    def workouts(self, i, day, tz, dist_unit, km_per):
        """Emit the day's workouts; return (active Cal, exercise min)."""
        rng = self.rng
        wd = day.weekday()
        cal = mins = 0.0
        if wd in (0, 2, 4):
            indoor = i in self.trip
            dur = rng.randrange(28, 50)
            km = dur / 6.0 * rng.uniform(0.9, 1.1)
            cal, mins = self.workout(i, day, tz, "Running", self.at(day, tz, 7), dur, km, dist_unit, km_per,
                                     route=not indoor, indoor=indoor)
        elif wd == 1:
            dur = rng.randrange(40, 60)
            cal, mins = self.workout(i, day, tz, "TraditionalStrengthTraining", self.at(day, tz, 18), dur, None,
                                     dist_unit, km_per)
            t = self.at(day, tz, 18)
            self.q("ActiveEnergyBurned", "Synthetic Gym App", t, t + timedelta(minutes=dur), tz,
                   dur * 6 * 4.184, "kJ", 0)
        elif wd == 5:
            dur = rng.randrange(60, 95)
            km = dur / 60 * rng.uniform(22, 27)
            cal, mins = self.workout(i, day, tz, "Cycling", self.at(day, tz, 9), dur, km, dist_unit, km_per,
                                     route=True, dist_type="DistanceCycling")
        elif wd == 6:
            # Legacy self-closing walk with totals on the element itself.
            dur = rng.randrange(25, 45)
            s = self.at(day, tz, 10)
            e = s + timedelta(minutes=dur)
            dist = dur / 12 * km_per
            kcal = dur * 4
            self.lines.append(
                f' <Workout workoutActivityType="HKWorkoutActivityTypeWalking" duration="{dur}" durationUnit="min" '
                f'totalDistance="{num(dist, 3)}" totalDistanceUnit="{dist_unit}" totalEnergyBurned="{kcal}" '
                f'totalEnergyBurnedUnit="kcal" sourceName="{PHONE}" sourceVersion="19.0" '
                f'creationDate="{stamp(e, tz)}" startDate="{stamp(s, tz)}" endDate="{stamp(e, tz)}"/>'
            )
        if i == self.midnight_walk:
            self.workout(i, day, tz, "Walking", self.at(day, tz, 23, 40), 40, 3.2, dist_unit, km_per)
        return cal, mins

    def workout(self, i, day, tz, activity, start, dur, km, dist_unit, km_per, route=False, indoor=False,
                dist_type="DistanceWalkingRunning"):
        rng = self.rng
        end = start + timedelta(minutes=dur)
        kcal = dur * rng.uniform(8, 11)
        L = self.lines
        L.append(
            f' <Workout workoutActivityType="HKWorkoutActivityType{activity}" duration="{dur}" durationUnit="min" '
            f'sourceName="{WATCH}" sourceVersion="12.0" device="{attr(SOURCES[WATCH][1])}" '
            f'creationDate="{stamp(end + timedelta(minutes=1), tz)}" startDate="{stamp(start, tz)}" '
            f'endDate="{stamp(end, tz)}">'
        )
        L.append(f'  <MetadataEntry key="HKIndoorWorkout" value="{1 if indoor else 0}"/>')
        L.append(f'  <MetadataEntry key="HKTimeZone" value="{tz.key}"/>')
        if rng.random() < 0.3:
            p = start + timedelta(minutes=dur // 2)
            L.append(f'  <WorkoutEvent type="HKWorkoutEventTypePause" date="{stamp(p, tz)}"/>')
            L.append(f'  <WorkoutEvent type="HKWorkoutEventTypeResume" date="{stamp(p + timedelta(minutes=1), tz)}"/>')
        L.append(
            f'  <WorkoutStatistics type="HKQuantityTypeIdentifierActiveEnergyBurned" startDate="{stamp(start, tz)}" '
            f'endDate="{stamp(end, tz)}" sum="{num(kcal, 1)}" unit="kcal"/>'
        )
        hr = [rng.gauss(148, 9) for _ in range(dur)]
        L.append(
            f'  <WorkoutStatistics type="HKQuantityTypeIdentifierHeartRate" startDate="{stamp(start, tz)}" '
            f'endDate="{stamp(end, tz)}" average="{num(sum(hr) / len(hr), 1)}" minimum="{num(min(hr), 0)}" '
            f'maximum="{num(max(hr), 0)}" unit="count/min"/>'
        )
        if km is not None:
            L.append(
                f'  <WorkoutStatistics type="HKQuantityTypeIdentifier{dist_type}" startDate="{stamp(start, tz)}" '
                f'endDate="{stamp(end, tz)}" sum="{num(km * km_per, 3)}" unit="{dist_unit}"/>'
            )
        if route:
            local = start.astimezone(tz)
            name = f"route_{local:%Y-%m-%d}_{local.hour % 12 or 12}.{local:%M}{'am' if local.hour < 12 else 'pm'}.gpx"
            self.routes[name] = self.gpx(start, dur, km)
            L.append(
                f'  <WorkoutRoute sourceName="{WATCH}" sourceVersion="12.0" creationDate="{stamp(end, tz)}" '
                f'startDate="{stamp(start, tz)}" endDate="{stamp(end, tz)}">'
            )
            L.append(f'   <MetadataEntry key="HKMetadataKeySyncIdentifier" value="00000000-0000-0000-0000-{i:012d}"/>')
            L.append(f'   <FileReference path="/workout-routes/{name}"/>')
            L.append("  </WorkoutRoute>")
        L.append(" </Workout>")

        for k in range(dur):
            t = start + timedelta(minutes=k)
            self.q("HeartRate", WATCH, t, t, tz, hr[k], "count/min", 0, (("HKMetadataKeyHeartRateMotionContext", "2"),))
        self.q("ActiveEnergyBurned", WATCH, start, end, tz, kcal, "kcal", 1)
        self.q("AppleExerciseTime", WATCH, start, end, tz, dur, "min", 0)
        if km is not None:
            self.q(dist_type, WATCH, start, end, tz, km * km_per, dist_unit, 3)
        return kcal, dur

    def gpx(self, start, dur, km):
        rng = self.rng
        lat0 = ROUTE_BASE[0] + rng.uniform(-0.02, 0.02)
        lon0 = ROUTE_BASE[1] + rng.uniform(-0.02, 0.02)
        radius_m = km * 1000 / (2 * math.pi)
        n = dur * 6  # a point every 10 s
        speed = km * 1000 / (dur * 60)
        pts = []
        for k in range(n + 1):
            a = 2 * math.pi * k / n
            lat = lat0 + radius_m * math.sin(a) / 111_320
            lon = lon0 + radius_m * (1 - math.cos(a)) / (111_320 * math.cos(math.radians(lat0)))
            t = (start + timedelta(seconds=10 * k)).strftime("%Y-%m-%dT%H:%M:%SZ")
            h_acc = 48.0 if k == n // 3 else 4.0  # one poor-accuracy fix
            pts.append(
                f'   <trkpt lon="{lon:.6f}" lat="{lat:.6f}"><ele>{20 + 5 * math.sin(3 * a):.1f}</ele>'
                f"<time>{t}</time><extensions><speed>{speed:.2f}</speed>"
                f"<course>{math.degrees(a) % 360:.1f}</course><hAcc>{h_acc}</hAcc><vAcc>3.0</vAcc>"
                f"</extensions></trkpt>"
            )
        return (
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            '<!-- Synthetic workout route. No real location data. -->\n'
            '<gpx version="1.1" creator="Apple Health Export" xmlns="http://www.topografix.com/GPX/1/1">\n'
            f" <metadata>\n  <time>{start.strftime('%Y-%m-%dT%H:%M:%SZ')}</time>\n </metadata>\n"
            " <trk>\n  <trkseg>\n" + "\n".join(pts) + "\n  </trkseg>\n </trk>\n</gpx>\n"
        )

    # ── output ────────────────────────────────────────────────────────────

    def write(self, out):
        for i in range(self.days):
            self.day(i)
        root = os.path.join(out, "apple_health_export")
        os.makedirs(os.path.join(root, "workout-routes"), exist_ok=True)
        exported = datetime(self.end.year, self.end.month, self.end.day, 9, tzinfo=HOME_TZ) + timedelta(days=1)
        xml = os.path.join(root, "export.xml")
        with open(xml, "w", encoding="utf-8") as f:
            f.write(
                '<?xml version="1.0" encoding="UTF-8"?>\n'
                "<!-- Synthetic Apple Health export from scripts/synth-export.py. No real data. -->\n"
                "<!DOCTYPE HealthData [\n"
                "<!ELEMENT HealthData (ExportDate,Me,(Record|Correlation|Workout|ActivitySummary)*)>\n"
                "<!ATTLIST HealthData locale CDATA #REQUIRED>\n"
                "]>\n"
                '<HealthData locale="en_US">\n'
                f' <ExportDate value="{exported.strftime("%Y-%m-%d %H:%M:%S %z")}"/>\n'
                ' <Me HKCharacteristicTypeIdentifierDateOfBirth="2000-01-01" '
                'HKCharacteristicTypeIdentifierBiologicalSex="HKBiologicalSexNotSet"/>\n'
            )
            f.write("\n".join(self.lines + self.summaries))
            f.write("\n</HealthData>\n")
        for name, text in self.routes.items():
            with open(os.path.join(root, "workout-routes", name), "w", encoding="utf-8") as f:
                f.write(text)
        return xml


def inside_repo(path):
    repo = os.path.realpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
    target = os.path.realpath(path)
    return target == repo or target.startswith(repo + os.sep)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--out", required=True, help="directory to write into (outside the repo)")
    p.add_argument("--days", type=int, default=240, help="days of history (default 240)")
    p.add_argument("--end", type=date.fromisoformat, default=date.today() - timedelta(days=1),
                   help="last day, YYYY-MM-DD (default yesterday)")
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--zip", action="store_true", help="also write export.zip, like the Health app does")
    args = p.parse_args()
    if args.days < 14:
        p.error("--days must be at least 14 so every edge case fits")
    if inside_repo(args.out):
        p.error(f"refusing to write inside the repository: {args.out}")

    ex = Export(args)
    xml = ex.write(args.out)
    print(f"{xml}: {len(ex.lines)} lines, {len(ex.routes)} routes, {ex.start}..{ex.end}")
    if args.zip:
        z = os.path.join(args.out, "export.zip")
        root = os.path.dirname(xml)
        with zipfile.ZipFile(z, "w", zipfile.ZIP_DEFLATED) as zf:
            for dirpath, _, files in os.walk(root):
                for fn in sorted(files):
                    full = os.path.join(dirpath, fn)
                    zf.write(full, os.path.relpath(full, args.out))
        print(z)


if __name__ == "__main__":
    sys.exit(main())
