#!/usr/bin/env python3
"""A stand-in for the Oura API v2, serving synthetic documents only.

Used by scripts/dev-db.sh (and so `make smoke`) to run the real
`vitals oura sync` end to end without an Oura account. Nothing here is real
data: every value comes from a seeded random generator.

    python3 -I scripts/oura-mock.py --days 60 --port-file /path/port

It listens on 127.0.0.1 on a free port, writes the port to --port-file, and
serves until killed:

- POST /oauth/token: any refresh_token → a new synthetic pair.
- GET /v2/usercollection/<kind>?start_date&end_date[&next_token]: that
  kind's documents with a day in range (end exclusive), 25 per page.

Two edge cases on purpose: some nights have a nap besides the main sleep,
and one day per week has no readiness (the ring was off).
"""

import argparse
import datetime as dt
import json
import random
import secrets
import socketserver
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

PAGE = 25


def build(days: int, seed: int, end: dt.date) -> dict:
    rnd = random.Random(seed)
    docs = {k: [] for k in (
        "daily_readiness", "daily_sleep", "sleep", "daily_activity", "daily_stress",
        "daily_spo2", "daily_cardiovascular_age", "vO2_max")}
    n = 0

    def nid() -> str:
        nonlocal n
        n += 1
        return f"synthetic-{n:06d}"

    for i in range(days):
        day = end - dt.timedelta(days=days - 1 - i)
        d = day.isoformat()
        if day.weekday() != 2:
            docs["daily_readiness"].append({
                "id": nid(), "day": d, "score": rnd.randint(55, 95),
                "temperature_deviation": round(rnd.uniform(-0.6, 0.6), 2)})
        docs["daily_sleep"].append({"id": nid(), "day": d, "score": rnd.randint(55, 95)})
        total = rnd.randint(5 * 3600, 8 * 3600)
        deep, rem = int(total * 0.18), int(total * 0.22)
        docs["sleep"].append({
            "id": nid(), "day": d, "type": "long_sleep",
            "bedtime_start": f"{(day - dt.timedelta(days=1)).isoformat()}T23:10:00-08:00",
            "bedtime_end": f"{d}T07:05:00-08:00",
            "total_sleep_duration": total, "deep_sleep_duration": deep,
            "rem_sleep_duration": rem, "light_sleep_duration": total - deep - rem,
            "awake_time": rnd.randint(900, 3600), "efficiency": rnd.randint(78, 96),
            "lowest_heart_rate": rnd.randint(44, 58), "average_heart_rate": rnd.randint(50, 64),
            "average_hrv": rnd.randint(25, 70), "average_breath": round(rnd.uniform(13, 16), 1)})
        if i % 9 == 4:
            docs["sleep"].append({
                "id": nid(), "day": d, "type": "late_nap", "total_sleep_duration": 1500,
                "average_hrv": 20})
        docs["daily_activity"].append({
            "id": nid(), "day": d, "score": rnd.randint(50, 98), "steps": rnd.randint(2500, 16000),
            "active_calories": rnd.randint(150, 900)})
        docs["daily_stress"].append({
            "id": nid(), "day": d, "stress_high": rnd.randint(0, 6) * 900,
            "recovery_high": rnd.randint(0, 8) * 900, "day_summary": "normal"})
        docs["daily_spo2"].append({
            "id": nid(), "day": d, "spo2_percentage": {"average": round(rnd.uniform(94, 99), 1)},
            "breathing_disturbance_index": rnd.randint(0, 12)})
        if i % 7 == 0:
            docs["daily_cardiovascular_age"].append(
                {"id": nid(), "day": d, "vascular_age": rnd.randint(30, 45)})
            docs["vO2_max"].append(
                {"id": nid(), "day": d, "vo2_max": round(rnd.uniform(38, 50), 1)})
    return docs


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=60)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--end", help="last day, YYYY-MM-DD (default today)")
    ap.add_argument("--port-file", required=True)
    a = ap.parse_args()
    end = dt.date.fromisoformat(a.end) if a.end else dt.date.today()
    docs = build(a.days, a.seed, end)

    class H(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def send(self, code: int, body: dict) -> None:
            raw = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_POST(self):
            if urlparse(self.path).path != "/oauth/token":
                return self.send(404, {"detail": "not found"})
            self.rfile.read(int(self.headers.get("content-length", 0)))
            self.send(200, {
                "access_token": "synthetic-" + secrets.token_hex(8),
                "refresh_token": "synthetic-" + secrets.token_hex(8),
                "expires_in": 86400, "token_type": "bearer"})

        def do_GET(self):
            u = urlparse(self.path)
            kind = u.path.rsplit("/", 1)[-1]
            if not u.path.startswith("/v2/usercollection/") or kind not in docs:
                return self.send(404, {"detail": "not found"})
            if not self.headers.get("authorization", "").startswith("Bearer "):
                return self.send(401, {"detail": "no token"})
            if not self.headers.get("user-agent"):
                return self.send(403, {"detail": "user agent required"})
            q = {k: v[0] for k, v in parse_qs(u.query).items()}
            lo, hi = q.get("start_date", "0000"), q.get("end_date", "9999")
            rows = [d for d in docs[kind] if lo <= d["day"] < hi]
            off = int(q.get("next_token", "0"))
            page = rows[off:off + PAGE]
            nxt = str(off + PAGE) if off + PAGE < len(rows) else None
            self.send(200, {"data": page, "next_token": nxt})

    class Server(ThreadingHTTPServer):
        def server_bind(self):
            # HTTPServer's own server_bind looks up the host's FQDN, which
            # can stall for half a minute; loopback needs no name.
            socketserver.TCPServer.server_bind(self)
            self.server_name, self.server_port = "127.0.0.1", self.server_address[1]

    srv = Server(("127.0.0.1", 0), H)
    with open(a.port_file, "w") as f:
        f.write(str(srv.server_address[1]))
    srv.serve_forever()


if __name__ == "__main__":
    main()
