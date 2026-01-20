import csv
import math
import random
from datetime import date, timedelta

# Paths
ROOT = "barter/examples/data"
MARKS_PATH = f"{ROOT}/bond_marks_template.csv"
SIGNALS_PATH = f"{ROOT}/bond_signals_template.csv"

start_date = date(2025, 1, 1)
end_date = date(2026, 1, 1)

# ---------------------------------------------------------
# 1. Generate MARKS (Daily, Clean Price)
# ---------------------------------------------------------
# Simulation params
s = 0.80  # initial spread
k = 0.95  # mean reversion speed
shock_size = 1.50

# Define specific dates where we will inject a "widening" shock 
# so our PAIR strategy has something to trade against.
entry_dates = [
    date(2025,1,15),
    date(2025,3,10),
    date(2025,5,5),
    date(2025,6,20),
    date(2025,8,15),
    date(2025,10,1),
    date(2025,11,20),
    date(2025,12,10),
]
shock_map = {d: shock_size for d in entry_dates}

rows_marks = []
cur = start_date
t = 0

# We need to track valid trading days for signals to align with marks
valid_dates = set()

while cur <= end_date:
    valid_dates.add(cur)
    
    # 1. Spread dynamics
    # Add shock if today is an entry date
    if cur in shock_map:
        s += shock_map[cur]
    
    # Mean reversion towards long-term mean (e.g. 0.50)
    # s_t = s_{t-1} + k * (mean - s_{t-1}) + noise
    s += k * (0.50 - s) + random.gauss(0, 0.05)
    
    # 2. Base rate / price level dynamics (random walk + drift)
    # math.sin to create some waves
    base = 100.0 + 2.0 * math.sin(t / 60.0) + (t / 365.0) * 2.0
    
    # 3. Derive bond prices from Base +/- Spread
    # idiosyncratic noise
    noise_a = random.gauss(0, 0.02)
    noise_b = random.gauss(0, 0.02)
    
    price_a = base - (s / 2.0) + noise_a
    price_b = base + (s / 2.0) + noise_b
    
    # Clamp to realistic bond prices
    price_a = max(70.0, min(130.0, price_a))
    price_b = max(70.0, min(130.0, price_b))
    
    rows_marks.append((cur, "BOND_A", "CLEAN_PRICE", f"{price_a:.4f}"))
    rows_marks.append((cur, "BOND_B", "CLEAN_PRICE", f"{price_b:.4f}"))
    
    cur += timedelta(days=1)
    t += 1

print(f"Generated {len(rows_marks)} mark rows.")

with open(MARKS_PATH, "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["ts", "security_id", "mark_type", "mark"])
    for r in rows_marks:
        w.writerow([r[0].isoformat(), r[1], r[2], r[3]])

# ---------------------------------------------------------
# 2. Generate SIGNALS (Sparse PAIR trades only)
# ---------------------------------------------------------
# Strategy:
# On 'entry_date': BUY BOND_A (cheap), SELL BOND_B (rich) -> betting spread narrows.
# On 'entry_date + 10 days': CLOSE both.
#
# Weights: +0.02 for Long, -0.02 for Short (Market Neutral-ish)

rows_signals = []
trade_count = 0

for i, entry_d in enumerate(entry_dates):
    # Validate date is in range
    if entry_d > end_date:
        continue
        
    exit_d = entry_d + timedelta(days=10)
    if exit_d > end_date:
        exit_d = end_date # force close at end
        
    trade_id = f"PAIR_{i+1:03d}"
    
    # ENTRY (Long A, Short B)
    # Note: Our generic backtester applies delta at close of 'ts' for return on 'ts+1',
    # OR we can assume fill_mark_override handles the entry execution price.
    # We'll leave fill_mark_override empty to just use Close Price for simplicity,
    # or we could simulate a 'fill' slightly worse than close. Let's keep it simple (None).
    
    # A: +2%
    rows_signals.append([entry_d.isoformat(), trade_id, "BOND_A", "0.05", ""])
    # B: -2%
    rows_signals.append([entry_d.isoformat(), trade_id, "BOND_B", "-0.05", ""])
    
    # EXIT (Close A, Close B) - Reverse signs
    rows_signals.append([exit_d.isoformat(), trade_id, "BOND_A", "-0.05", ""])
    rows_signals.append([exit_d.isoformat(), trade_id, "BOND_B", "0.05", ""])
    
    trade_count += 1

print(f"Generated {len(rows_signals)} signal rows ({trade_count} trades).")

with open(SIGNALS_PATH, "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["ts", "trade_id", "security_id", "delta_weight", "fill_mark_override"])
    w.writerows(rows_signals)
