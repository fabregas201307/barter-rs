import csv
import math
import random
import string
from collections import defaultdict
from datetime import date, timedelta
from pathlib import Path

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
ROOT = Path("barter/examples/data")
MARKS_PATH = ROOT / "bond_marks_shares.csv"
SIGNALS_PATH = ROOT / "bond_signals_shares.csv"

START_DATE = date(2025, 1, 1)
END_DATE = date(2026, 1, 1)
NUM_TRADES = 500  # number of pair trades (each produces two legs)
SECURITIES_PER_TRADE = 2
TOTAL_SECURITIES = NUM_TRADES * SECURITIES_PER_TRADE
HOLD_MIN_DAYS = 5
HOLD_MAX_DAYS = 20
BOOK_CAPITAL = 1_000_000
WEIGHT_MIN = 0.03  # 3% of book
WEIGHT_MAX = 0.07  # 7% of book
MIN_SHARES = 1
RANDOM_SEED = 42

random.seed(RANDOM_SEED)

# -----------------------------------------------------------------------------
# Helpers
# -----------------------------------------------------------------------------
def generate_security_id():
    return "".join(random.choices(string.digits, k=9))


def generate_trade_suffix():
    return "".join(random.choices(string.digits, k=4))


def round_shares(notional, price):
    if price <= 0:
        return MIN_SHARES
    shares = max(MIN_SHARES, int(round(notional / price)))
    return shares


# -----------------------------------------------------------------------------
# Build Universe
# -----------------------------------------------------------------------------
security_ids = set()
while len(security_ids) < TOTAL_SECURITIES:
    security_ids.add(generate_security_id())
security_ids = list(security_ids)
random.shuffle(security_ids)

security_pairs = [
    (security_ids[i], security_ids[i + 1])
    for i in range(0, TOTAL_SECURITIES, SECURITIES_PER_TRADE)
]

if len(security_pairs) != NUM_TRADES:
    raise RuntimeError("security pairing failed to match NUM_TRADES")

security_ids.sort()

security_params = {}
for sec_id in security_ids:
    security_params[sec_id] = {
        "base_price": 100.0 + random.uniform(-10, 10),
        "drift_speed": random.uniform(0.5, 2.0),
        "volatility": random.uniform(0.01, 0.05),
        "phase_shift": random.uniform(0, 2 * math.pi),
    }

# -----------------------------------------------------------------------------
# Generate Marks + Price Cache (for fills)
# -----------------------------------------------------------------------------
rows_marks = []
price_cache = defaultdict(dict)
cur = START_DATE
t = 0

print(f"Generating marks for {TOTAL_SECURITIES} securities...")

while cur <= END_DATE:
    market_factor = 2.0 * math.sin(t / 120.0) + (t / 365.0)

    for sec_id in security_ids:
        params = security_params[sec_id]
        idio_drift = params["drift_speed"] * math.sin(t / 60.0 + params["phase_shift"])
        noise = random.gauss(0, params["volatility"])
        price = params["base_price"] + market_factor + idio_drift + noise
        price = max(40.0, min(150.0, price))

        rows_marks.append((cur.isoformat(), sec_id, "CLEAN_PRICE", f"{price:.4f}"))
        price_cache[cur][sec_id] = price

    cur += timedelta(days=1)
    t += 1

print(f"Generated {len(rows_marks)} mark rows. Writing {MARKS_PATH}...")
MARKS_PATH.parent.mkdir(parents=True, exist_ok=True)
with MARKS_PATH.open("w", newline="") as f:
    writer = csv.writer(f)
    writer.writerow(["ts", "security_id", "mark_type", "mark"])
    writer.writerows(rows_marks)

# -----------------------------------------------------------------------------
# Generate Signals with share counts
# -----------------------------------------------------------------------------
rows_signals = []
valid_dates = sorted(price_cache.keys())
print(f"Generating {NUM_TRADES} pair trades with explicit share sizes...")

for trade_index in range(NUM_TRADES):
    entry_idx = random.randint(0, len(valid_dates) - HOLD_MAX_DAYS - 1)
    entry_date = valid_dates[entry_idx]
    holding_days = random.randint(HOLD_MIN_DAYS, HOLD_MAX_DAYS)
    exit_date = valid_dates[entry_idx + holding_days]
    weight = random.uniform(WEIGHT_MIN, WEIGHT_MAX)

    sec_long, sec_short = security_pairs[trade_index]
    trade_id = (
        f"{entry_date.strftime('%Y%m%d')}_{sec_long}_{sec_short}_{generate_trade_suffix()}"
    )

    long_price = price_cache[entry_date][sec_long]
    short_price = price_cache[entry_date][sec_short]
    long_shares = round_shares(BOOK_CAPITAL * weight, long_price)
    short_shares = round_shares(BOOK_CAPITAL * weight, short_price)

    rows_signals.append(
        [entry_date.isoformat(), trade_id, sec_long, long_shares, f"{long_price:.4f}"]
    )
    rows_signals.append(
        [entry_date.isoformat(), trade_id, sec_short, -short_shares, f"{short_price:.4f}"]
    )

    exit_long_price = price_cache[exit_date][sec_long]
    exit_short_price = price_cache[exit_date][sec_short]
    rows_signals.append(
        [exit_date.isoformat(), trade_id, sec_long, -long_shares, f"{exit_long_price:.4f}"]
    )
    rows_signals.append(
        [exit_date.isoformat(), trade_id, sec_short, short_shares, f"{exit_short_price:.4f}"]
    )

rows_signals.sort(key=lambda row: row[0])
print(f"Generated {len(rows_signals)} signal rows. Writing {SIGNALS_PATH}...")
with SIGNALS_PATH.open("w", newline="") as f:
    writer = csv.writer(f)
    writer.writerow(["ts", "trade_id", "security_id", "delta_shares", "fill_price"])
    writer.writerows(rows_signals)

print("Done. Use these CSVs for the share-based backtest.")
